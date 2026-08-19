use glua_parser::{
    LuaAssignStat, LuaAstNode, LuaAstToken, LuaCallExpr, LuaChunk, LuaClosureExpr, LuaExpr,
    LuaForRangeStat, LuaFuncStat, LuaIndexExpr, LuaLocalFuncStat, LuaLocalStat, LuaNameExpr,
    LuaReturnStat, LuaSyntaxId, LuaSyntaxNode, LuaTableExpr, LuaTableField, LuaVarExpr, PathTrait,
};
use rowan::TextSize;
use std::sync::Arc;

use super::{
    InferFailReason, InferResult, infer_expr, infer_table_field_value_should_be,
    infer_table_should_be,
};
use crate::{
    CacheEntry, FileId, GmodStateMask, LuaDecl, LuaDeclExtra, LuaDeclId, LuaInferCache,
    LuaMemberId, LuaMemberKey, LuaMemberOwner, LuaSemanticDeclId, LuaType, LuaTypeDeclId,
    SemanticDeclLevel, TypeOps,
    compilation::analyzer::{
        gmod::{get_scripted_class_type_decl_id, name_expr_resolves_to_scoped_authoring_table},
        infer_for_range_iter_expr_func,
    },
    db_index::{DbIndex, LuaDeclOrMemberId, LuaSignature, LuaSignatureId},
    infer_node_semantic_decl,
    semantic::{
        infer::narrow::{
            SelfRefId, VarRefId, infer_expr_narrow_type, infer_expr_narrow_type_with_self_base,
            remove_false_or_nil,
        },
        member::{
            find_members_with_key, find_members_with_key_in_workspace_for_file_at_offset,
            merge_open_table_types, visible_super_types_in_workspace_for_file_at_offset,
        },
        resolve_registered_vgui_method_context_for_func,
        semantic_info::resolve_global_decl_id,
    },
};

pub fn infer_name_expr(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    name_expr: LuaNameExpr,
) -> InferResult {
    let name_token = name_expr.get_name_token().ok_or(InferFailReason::None)?;
    let name = name_token.get_name_text();
    match name {
        "self" => {
            return infer_self(db, cache, name_expr);
        }
        "_G" | "_ENV" => return Ok(LuaType::Global),
        _ => {}
    }

    let file_id = cache.get_file_id();
    let references_index = db.get_reference_index();
    let range = name_expr.get_range();
    let decl_id = references_index
        .get_local_reference(&file_id)
        .and_then(|file_ref| file_ref.get_decl_id(&range));

    let reads_local = decl_id.is_some_and(|id| {
        db.get_decl_index()
            .get_decl(&id)
            .is_some_and(|decl| decl.is_local())
    });
    // `local x = foo` cannot see a `foo` declared further down the same file,
    // so those declarations are filtered out of every global lookup below —
    // including the one the resolved `decl_id` would otherwise point at.
    let skip_later_decls_in_current_file =
        !reads_local && reads_global_declared_later_in_file(db, file_id, &name_expr, name);
    let decl_id = decl_id.filter(|id| {
        !(skip_later_decls_in_current_file
            && id.file_id == file_id
            && id.position > name_expr.get_position())
    });

    let result = if let Some(decl_id) = decl_id {
        if db
            .get_decl_index()
            .get_decl(&decl_id)
            .is_some_and(LuaDecl::is_global)
            && let Some(class_decl_id) =
                resolve_global_singleton_scripted_type_decl_id(db, cache, name)
        {
            Ok(LuaType::Def(class_decl_id))
        } else {
            infer_local_decl_name_type(db, cache, &name_expr, decl_id)
        }
    } else {
        if let Some(implicit_module_type) =
            infer_legacy_module_implicit_type(db, file_id, name_expr.get_position(), name)
        {
            return Ok(implicit_module_type);
        }

        if let Some(define_baseclass_type) = infer_define_baseclass_type(db, file_id, name) {
            return Ok(define_baseclass_type);
        }

        if let Some(class_decl_id) = resolve_global_singleton_scripted_type_decl_id(db, cache, name)
        {
            return Ok(LuaType::Def(class_decl_id));
        }

        if let Some(class_decl_id) =
            name_expr_resolves_to_scoped_authoring_table(db, file_id, &name_expr)
        {
            return Ok(LuaType::Def(class_decl_id));
        }

        // The var-ref path narrows through the decl the reference index found,
        // which here is the very declaration this read cannot have seen, so it
        // would reintroduce what the filter removes. Go straight to the
        // filtered global lookup.
        if skip_later_decls_in_current_file {
            infer_global_type_impl(
                db,
                Some(file_id),
                Some(name_expr.get_position()),
                name,
                true,
            )
        } else {
            match get_name_expr_var_ref_id(db, cache, &name_expr) {
                Some(var_ref_id) => {
                    let narrow_res = infer_expr_narrow_type(
                        db,
                        cache,
                        LuaExpr::NameExpr(name_expr.clone()),
                        var_ref_id,
                    );
                    if decl_id.is_none() {
                        if let Ok(ref typ) = narrow_res {
                            let should_fallback =
                                typ.is_nil() || matches!(typ, LuaType::TableConst(_));
                            if should_fallback {
                                if let Ok(global_type) = infer_global_type(
                                    db,
                                    Some(file_id),
                                    Some(name_expr.get_position()),
                                    name,
                                ) {
                                    if global_type.is_custom_type()
                                        || (!global_type.is_nil()
                                            && !global_type.is_nullable()
                                            && !global_type.is_unknown()
                                            && !matches!(
                                                global_type,
                                                LuaType::Any | LuaType::Never
                                            ))
                                    {
                                        return Ok(global_type);
                                    }
                                }
                            }
                        }
                    }
                    narrow_res.or_else(|_| {
                        infer_global_type(db, Some(file_id), Some(name_expr.get_position()), name)
                    })
                }
                None => infer_global_type(db, Some(file_id), Some(name_expr.get_position()), name),
            }
        }
    };

    if let Some(decl_id) = decl_id
        && result.as_ref().is_ok_and(|typ| typ.contain_tpl())
        && let Some(iter_type) =
            try_infer_enclosing_for_range_iter_type(db, cache, &name_expr, decl_id)
    {
        return Ok(iter_type);
    }

    // When the inferred type contains unresolved SelfInfer (e.g. from
    // `local selfTbl = GetTable(self)` where the call's SelfInfer wasn't
    // resolved during compilation), resolve it using the enclosing method's
    // self type.
    if let Ok(ref typ) = result {
        if contains_self_infer(typ) {
            if let Some(self_type) = infer_enclosing_self_type(db, cache, &name_expr) {
                return Ok(resolve_self_infer(typ, &self_type));
            }
        }
    }

    result
}

fn resolve_global_singleton_scripted_type_decl_id(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    name: &str,
) -> Option<LuaTypeDeclId> {
    if cache.scripted_global_singleton_type_cache.is_none() {
        let emmyrc = db.get_emmyrc();
        let mut lookup = rustc_hash::FxHashMap::default();

        if emmyrc.gmod.enabled {
            for definition in emmyrc
                .gmod
                .scripted_class_scopes
                .resolved_definitions_slice()
                .iter()
                .filter(|definition| definition.is_global_singleton)
            {
                let class_name = definition
                    .fixed_class_name
                    .as_deref()
                    .unwrap_or(&definition.class_global);
                let type_decl_id =
                    get_scripted_class_type_decl_id(&definition.class_global, class_name);
                lookup.insert(definition.class_global.clone(), type_decl_id.clone());
                for alias in &definition.aliases {
                    lookup.insert(alias.clone(), type_decl_id.clone());
                }
            }
        }

        cache.scripted_global_singleton_type_cache = Some(lookup);
    }

    cache
        .scripted_global_singleton_type_cache
        .as_ref()
        .and_then(|lookup| lookup.get(name))
        .cloned()
}

fn infer_local_decl_name_type(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    name_expr: &LuaNameExpr,
    decl_id: LuaDeclId,
) -> InferResult {
    let var_ref_id = VarRefId::VarRef(decl_id);
    let result =
        infer_expr_narrow_type(db, cache, LuaExpr::NameExpr(name_expr.clone()), var_ref_id);

    let file_id = cache.get_file_id();
    let is_global = db
        .get_decl_index()
        .get_decl(&decl_id)
        .is_some_and(|decl| decl.is_global());

    if is_global {
        if let Ok(ref typ) = result {
            let should_fallback = typ.is_nil() || matches!(typ, LuaType::TableConst(_));
            if should_fallback {
                if let Some(name_token) = name_expr.get_name_token() {
                    let name = name_token.get_name_text();
                    if let Ok(global_type) =
                        infer_global_type(db, Some(file_id), Some(name_expr.get_position()), name)
                    {
                        if global_type.is_custom_type()
                            || (!global_type.is_nil()
                                && !global_type.is_nullable()
                                && !global_type.is_unknown()
                                && !matches!(global_type, LuaType::Any | LuaType::Never))
                        {
                            return Ok(global_type);
                        }
                    }
                }
            }
        }
    }

    if let Ok(typ) = &result
        && let Some(initializer_type) = try_local_decl_initializer_fallback_type(
            db,
            cache,
            decl_id,
            typ,
            name_expr.get_position(),
        )
    {
        return Ok(initializer_type);
    }

    result
}

fn try_infer_enclosing_for_range_iter_type(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    name_expr: &LuaNameExpr,
    decl_id: LuaDeclId,
) -> Option<LuaType> {
    if let Some(cached) = cache.for_range_iter_var_type_cache.get(&decl_id).cloned() {
        return match cached {
            CacheEntry::Cache(typ) => Some(typ),
            CacheEntry::Ready | CacheEntry::Error(_) => None,
        };
    }

    let for_range = name_expr
        .syntax()
        .ancestors()
        .find_map(LuaForRangeStat::cast)?;
    let var_idx = for_range
        .get_var_name_list()
        .enumerate()
        .find_map(|(idx, var_name)| (var_name.get_position() == decl_id.position).then_some(idx))?;
    let iter_exprs = for_range.get_expr_list().collect::<Vec<_>>();
    cache
        .for_range_iter_var_type_cache
        .insert(decl_id, CacheEntry::Ready);
    let iter_var_types = match infer_for_range_iter_expr_func(db, cache, &iter_exprs) {
        Ok(iter_var_types) => iter_var_types,
        Err(reason) => {
            cache
                .for_range_iter_var_type_cache
                .insert(decl_id, CacheEntry::Error(reason));
            return None;
        }
    };
    let ret_type = iter_var_types
        .get_type(var_idx)
        .cloned()
        .unwrap_or(LuaType::Unknown);
    let ret_type = TypeOps::Remove.apply(db, &ret_type, &LuaType::Nil);
    cache
        .for_range_iter_var_type_cache
        .insert(decl_id, CacheEntry::Cache(ret_type.clone()));

    Some(ret_type)
}

pub(crate) fn try_local_decl_initializer_fallback_type(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    decl_id: LuaDeclId,
    current_type: &LuaType,
    query_position: TextSize,
) -> Option<LuaType> {
    if current_type.is_never() {
        if has_local_reassignment_between(db, cache, decl_id, query_position) {
            return None;
        }

        return try_infer_local_initializer_type(db, cache, decl_id);
    }

    if let Some(alias_type) = try_infer_flow_sensitive_alias_initializer_type(
        db,
        cache,
        decl_id,
        current_type,
        query_position,
    ) {
        return Some(alias_type);
    }

    if !((current_type.is_unknown() || current_type.is_nil())
        && local_decl_type_cache_is_inferred(db, decl_id)
        && is_gmod_dynamic_initializer(db, decl_id))
    {
        return None;
    }

    if has_local_reassignment_between(db, cache, decl_id, query_position) {
        return None;
    }

    let initializer_type = try_infer_local_initializer_type(db, cache, decl_id)?;
    initializer_type_is_informative(&initializer_type).then_some(initializer_type)
}

/// Whether re-reading the declaration's initializer can improve on a narrowed
/// type. A type whose only content once the falsy arms are removed is `unknown`
/// says nothing the narrowed type does not already say, and substituting it
/// would put back the `nil` a guard has just discharged.
fn initializer_type_is_informative(initializer_type: &LuaType) -> bool {
    !initializer_type.is_never()
        && !initializer_type.is_nil()
        && !remove_false_or_nil(initializer_type.clone()).is_unknown()
}

fn try_infer_flow_sensitive_alias_initializer_type(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    decl_id: LuaDeclId,
    current_type: &LuaType,
    query_position: TextSize,
) -> Option<LuaType> {
    if !(current_type.is_unknown() || current_type.is_nil() || current_type.is_table()) {
        return None;
    }

    if has_local_reassignment_between(db, cache, decl_id, query_position) {
        return None;
    }

    let (_, initializer_expr) = local_initializer_expr(db, decl_id)?;
    if !matches!(
        initializer_expr,
        LuaExpr::NameExpr(_) | LuaExpr::IndexExpr(_)
    ) {
        return None;
    }

    let initializer_ref =
        super::narrow::get_var_expr_var_ref_id(db, cache, initializer_expr.clone())?;
    if !db.get_flow_index().has_special_call_effect_before(
        &decl_id.file_id,
        decl_id.position,
        &initializer_ref,
    ) {
        return None;
    }

    let initializer_type = infer_expr(db, cache, initializer_expr).ok()?;
    (initializer_type_is_informative(&initializer_type) && !initializer_type.is_table())
        .then_some(initializer_type)
}

fn has_local_reassignment_between(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    decl_id: LuaDeclId,
    query_position: TextSize,
) -> bool {
    if query_position <= decl_id.position {
        return false;
    }

    if !cache.local_reassignments_indexed {
        collect_local_reassignment_positions(db, cache);
    }

    cache
        .local_reassignment_positions_cache
        .get(&decl_id)
        .and_then(|positions| positions.first())
        .is_some_and(|position| *position < query_position)
}

fn collect_local_reassignment_positions(db: &DbIndex, cache: &mut LuaInferCache) {
    cache.local_reassignments_indexed = true;
    let file_id = cache.get_file_id();
    let Some(root) = db
        .get_vfs()
        .get_syntax_tree(&file_id)
        .map(|tree| tree.get_red_root())
    else {
        return;
    };

    let references = db.get_reference_index().get_local_reference(&file_id);
    let decl_tree = db.get_decl_index().get_decl_tree(&file_id);
    for assign_stat in root.descendants().filter_map(LuaAssignStat::cast) {
        let position = assign_stat.get_position();

        let (vars, _) = assign_stat.get_var_and_expr_list();
        for var in vars {
            let LuaVarExpr::NameExpr(name_expr) = var else {
                continue;
            };

            let assigned_decl_id = references
                .and_then(|refs| refs.get_decl_id(&name_expr.get_range()))
                .or_else(|| assignment_name_decl_id(decl_tree, &name_expr));
            let Some(assigned_decl_id) = assigned_decl_id else {
                continue;
            };
            if assigned_decl_id.file_id != file_id || position <= assigned_decl_id.position {
                continue;
            }

            cache
                .local_reassignment_positions_cache
                .entry(assigned_decl_id)
                .or_default()
                .push(position);
        }
    }

    for positions in cache.local_reassignment_positions_cache.values_mut() {
        positions.sort_unstable();
        positions.dedup();
    }
}

fn assignment_name_decl_id(
    decl_tree: Option<&crate::LuaDeclarationTree>,
    name_expr: &LuaNameExpr,
) -> Option<LuaDeclId> {
    let name = name_expr.get_name_text()?;

    decl_tree
        .and_then(|tree| tree.find_local_decl(&name, name_expr.get_position()))
        .map(|decl| decl.get_id())
}

fn try_infer_local_initializer_type(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    decl_id: LuaDeclId,
) -> Option<LuaType> {
    let (initializer_ret_idx, expr) = local_initializer_expr(db, decl_id)?;
    let initializer_is_call = matches!(&expr, LuaExpr::CallExpr(_));
    let init_type = match infer_expr(db, cache, expr).ok()? {
        LuaType::Variadic(variadic) => variadic
            .get_type(initializer_ret_idx)
            .cloned()
            .unwrap_or(LuaType::Nil),
        ty if initializer_ret_idx == 0 => ty,
        LuaType::Unknown if initializer_is_call => LuaType::Unknown,
        _ => LuaType::Nil,
    };

    (!init_type.is_never() && !init_type.is_nil()).then_some(init_type)
}

fn local_initializer_expr(db: &DbIndex, decl_id: LuaDeclId) -> Option<(usize, LuaExpr)> {
    let decl = db.get_decl_index().get_decl(&decl_id)?;
    let initializer = decl.get_initializer()?;
    let root = db
        .get_vfs()
        .get_syntax_tree(&decl_id.file_id)?
        .get_red_root();
    let node = initializer.get_expr_syntax_id().to_node_from_root(&root)?;
    Some((initializer.get_ret_idx(), LuaExpr::cast(node)?))
}

fn local_decl_type_cache_is_inferred(db: &DbIndex, decl_id: LuaDeclId) -> bool {
    db.get_type_index()
        .get_type_cache(&decl_id.into())
        .is_some_and(|type_cache| type_cache.is_infer())
}

fn is_gmod_dynamic_initializer(db: &DbIndex, decl_id: LuaDeclId) -> bool {
    if !db.get_emmyrc().gmod.enabled || !db.get_emmyrc().gmod.infer_dynamic_fields {
        return false;
    }

    matches!(
        local_initializer_expr(db, decl_id),
        Some((_, LuaExpr::CallExpr(_) | LuaExpr::IndexExpr(_)))
    )
}

fn infer_define_baseclass_type(db: &DbIndex, file_id: FileId, name: &str) -> Option<LuaType> {
    if !db.get_emmyrc().gmod.enabled || name != "BaseClass" {
        return None;
    }

    let base_name = db
        .get_gmod_class_metadata_index()
        .get_define_baseclass_name(&file_id)?;
    Some(LuaType::Ref(LuaTypeDeclId::global(base_name)))
}

fn infer_self(db: &DbIndex, cache: &mut LuaInferCache, name_expr: LuaNameExpr) -> InferResult {
    let self_ref_id = match get_name_expr_var_ref_id(db, cache, &name_expr) {
        Some(VarRefId::SelfRef(self_ref_id)) => self_ref_id,
        _ => return Err(InferFailReason::None),
    };

    // Compute a region-aware base for the implicit `self` (the colon-method
    // receiver inferred at its own position). For reused locals reassigned per
    // region this yields the correct per-region class; for stable globals it
    // yields the same declared type the generic path would. We then run the
    // normal flow-narrowing pipeline on top of this base, so guards like
    // `if self == self.parent then ... end` still narrow correctly.
    //
    // The base is only used when concrete (Def/Ref/TableConst/Instance/...),
    // so generic `SelfInfer`/declared-parameter `self` still falls through to
    // the canonical `get_var_ref_type` resolution.
    let base_seed = infer_implicit_method_self_type(db, cache, &name_expr);

    infer_expr_narrow_type_with_self_base(
        db,
        cache,
        LuaExpr::NameExpr(name_expr),
        VarRefId::SelfRef(self_ref_id),
        base_seed,
    )
}

/// Resolves the type of an implicit `self` inside a colon method by binding it
/// to the method's receiver (the colon-method prefix), so `self` always agrees
/// with the parent it is defined within — including reused locals reassigned to
/// distinct tables/classes per region.
///
/// Resolution order:
/// 1. Scoped authoring tables (ENT/SWEP/GM) resolve by their scoped class
///    metadata (one class per file).
/// 2. Otherwise infer the enclosing colon-method prefix expression *at its
///    position* (region-aware via flow + GMod table-literal class binding) and
///    use it when it yields a concrete receiver type.
///
/// Returns `None` for explicit (non-implicit) `self`, or when no concrete
/// receiver type can be derived, so callers fall back to the generic
/// declaration/member `SelfRef` path.
fn infer_implicit_method_self_type(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    name_expr: &LuaNameExpr,
) -> Option<LuaType> {
    let file_id = cache.get_file_id();
    let decl_tree = db.get_decl_index().get_decl_tree(&file_id)?;
    let self_decl = decl_tree.find_local_decl("self", name_expr.get_position())?;
    if !self_decl.is_implicit_self() {
        return None;
    }

    let func_stat = name_expr.ancestors::<LuaFuncStat>().next()?;
    let func_syntax_id = func_stat.get_syntax_id();

    // Cache the unified result (including a negative result) for subsequent
    // `self` references in the same method body.
    if let Some(cached) = cache.self_type_cache.get(&func_syntax_id) {
        return cached.clone();
    }

    let result = infer_implicit_method_self_type_inner(db, cache, name_expr);
    cache.self_type_cache.insert(func_syntax_id, result.clone());
    result
}

fn infer_implicit_method_self_type_inner(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    name_expr: &LuaNameExpr,
) -> Option<LuaType> {
    let func_stat = enclosing_colon_func_stat(name_expr)?;
    if let Some(authoritative_type) = infer_authoritative_method_self_type(db, cache, &func_stat) {
        return Some(authoritative_type);
    }

    // A colon method selected as a raw table member can be invoked with an
    // explicit receiver (`callback(self, ...)`). Call-site analysis stores that
    // receiver in the synthetic slot immediately after the explicit params.
    if let Some(call_site_type) =
        infer_implicit_self_type_from_call_site(db, cache.get_file_id(), name_expr)
    {
        return Some(call_site_type);
    }

    // General case: infer the enclosing colon-method prefix at its position.
    //    This is region-aware, so a reused local resolves `self` to the class
    //    of the table backing the current region.
    let prefix_type = infer_method_prefix_type(db, cache, &func_stat)?;
    is_concrete_self_receiver_type(&prefix_type).then_some(prefix_type)
}

fn infer_implicit_self_type_from_call_site(
    db: &DbIndex,
    file_id: FileId,
    name_expr: &LuaNameExpr,
) -> Option<LuaType> {
    let closure = name_expr.ancestors::<LuaClosureExpr>().next()?;
    let signature_id = LuaSignatureId::from_closure(file_id, &closure);
    let signature = db.get_signature_index().get(&signature_id)?;
    if !signature.is_colon_define {
        return None;
    }
    db.get_call_site_param_index()
        .get_inferred_param(&signature_id, signature.params.len())
        .filter(|typ| is_concrete_self_receiver_type(typ))
        .cloned()
}

pub(crate) fn infer_authoritative_method_self_type(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    func_stat: &LuaFuncStat,
) -> Option<LuaType> {
    let func_name = func_stat.get_func_name()?;
    let LuaVarExpr::IndexExpr(index_expr) = func_name else {
        return None;
    };
    if !index_expr.get_index_token()?.is_colon() {
        return None;
    }

    if let Some(LuaExpr::NameExpr(prefix_name_expr)) = index_expr.get_prefix_expr()
        && let Some(class_decl_id) =
            name_expr_resolves_to_scoped_authoring_table(db, cache.get_file_id(), &prefix_name_expr)
    {
        return Some(LuaType::Def(class_decl_id));
    }

    if let Some(context) =
        resolve_registered_vgui_method_context_for_func(db, cache.get_file_id(), func_stat)
    {
        return Some(LuaType::Def(LuaTypeDeclId::global(&context.panel_name)));
    }

    let prefix_type = infer_method_prefix_type(db, cache, func_stat)?;
    is_authoritative_self_receiver_type(&prefix_type).then_some(prefix_type)
}

pub(crate) fn is_authoritative_self_receiver_type(typ: &LuaType) -> bool {
    match typ {
        LuaType::Def(_) | LuaType::Ref(_) => true,
        LuaType::Instance(instance) => is_authoritative_self_receiver_type(instance.get_base()),
        _ => false,
    }
}

/// Returns true when `typ` is a concrete receiver type suitable to be used
/// directly as an implicit `self` type. Rejects unconstrained/unknown types so
/// the caller falls back to the generic `SelfRef` resolution path (preserving
/// generic `SelfInfer` and declared-parameter behavior).
fn is_concrete_self_receiver_type(typ: &LuaType) -> bool {
    match typ {
        LuaType::Def(_)
        | LuaType::Ref(_)
        | LuaType::TableConst(_)
        | LuaType::Instance(_)
        | LuaType::Object(_) => true,
        LuaType::Union(union) => union.types().any(is_concrete_self_receiver_type),
        _ => false,
    }
}

pub fn get_name_expr_var_ref_id(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    name_expr: &LuaNameExpr,
) -> Option<VarRefId> {
    let syntax_id = name_expr.get_syntax_id();
    if let Some(var_ref_id) = cache.expr_var_ref_id_cache.get(&syntax_id) {
        return Some(var_ref_id.clone());
    }

    let name_token = name_expr.get_name_token()?;
    let name = name_token.get_name_text();
    let var_ref_id = match name {
        "self" => {
            let self_ref_id = find_self_ref_id(db, cache, name_expr)?;
            VarRefId::SelfRef(self_ref_id)
        }
        _ => {
            let file_id = cache.get_file_id();
            let references_index = db.get_reference_index();
            let range = name_expr.get_range();
            if let Some(decl_id) = references_index
                .get_local_reference(&file_id)
                .and_then(|file_ref| file_ref.get_decl_id(&range))
            {
                VarRefId::VarRef(decl_id)
            } else if let Some(global_decl_id) =
                resolve_global_decl_id(db, cache, name, Some(name_expr))
            {
                VarRefId::VarRef(global_decl_id)
            } else {
                VarRefId::GlobalName(
                    internment::ArcIntern::new(smol_str::SmolStr::new(name)),
                    name_expr.get_position(),
                )
            }
        }
    };

    cache
        .expr_var_ref_id_cache
        .insert(syntax_id, var_ref_id.clone());
    Some(var_ref_id)
}

pub fn infer_param(db: &DbIndex, decl: &LuaDecl) -> InferResult {
    infer_param_inner(db, None, decl).map(|(typ, _)| typ)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ParamInferenceSource {
    Explicit,
    Concrete,
    Contextual,
    NameFallback,
}

impl ParamInferenceSource {
    pub(crate) fn is_weak(self) -> bool {
        matches!(self, Self::Contextual | Self::NameFallback)
    }
}

fn infer_param_inner(
    db: &DbIndex,
    mut cache: Option<&mut LuaInferCache>,
    decl: &LuaDecl,
) -> Result<(LuaType, ParamInferenceSource), InferFailReason> {
    let (param_idx, signature_id, member_id) = match &decl.extra {
        LuaDeclExtra::Param {
            idx,
            signature_id,
            owner_member_id: closure_owner_syntax_id,
        } => (*idx, *signature_id, *closure_owner_syntax_id),
        _ => unreachable!(),
    };

    let mut colon_define = false;
    // find local annotation
    if let Some(signature) = db.get_signature_index().get(&signature_id) {
        colon_define = signature.is_colon_define;
        if let Some(param_info) = signature.get_param_info_by_id(param_idx) {
            let mut typ = param_info.type_ref.clone();
            if param_info.nullable && !typ.is_nullable() {
                typ = TypeOps::Union.apply(db, &typ, &LuaType::Nil);
            }

            typ = union_signature_overload_param_types(
                db,
                signature,
                typ,
                param_idx,
                colon_define,
                decl.get_name() == "...",
            );

            if let Some(member_id) = member_id
                && let Some(sibling_type) = find_param_type_from_sibling_members(
                    db,
                    member_id,
                    param_idx,
                    colon_define,
                    decl.get_name() == "...",
                    Some(&typ),
                )
            {
                typ = TypeOps::Union.apply(db, &typ, &sibling_type);
            }

            return Ok((typ, ParamInferenceSource::Explicit));
        }
    }

    if let Some(current_member_id) = member_id {
        let member_decl_type = find_decl_member_type(db, current_member_id)?;
        let param_type = find_param_type_from_type(
            db,
            member_decl_type,
            param_idx,
            colon_define,
            decl.get_name() == "...",
        );
        if let Some(param_type) = param_type {
            return Ok((param_type, ParamInferenceSource::Concrete));
        }

        if let Some(param_type) = find_param_type_from_inherited_members(
            db,
            current_member_id,
            param_idx,
            colon_define,
            decl.get_name() == "...",
            decl.get_file_id(),
            decl.get_position(),
        ) {
            return Ok((param_type, ParamInferenceSource::Concrete));
        }

        if let Some(param_type) = find_param_type_from_sibling_members(
            db,
            current_member_id,
            param_idx,
            colon_define,
            decl.get_name() == "...",
            None,
        ) {
            return Ok((param_type, ParamInferenceSource::Concrete));
        }

        if let Some(cache) = cache.as_mut()
            && let Some(param_type) = find_param_type_from_contextual_member(
                db,
                cache,
                current_member_id,
                param_idx,
                colon_define,
                decl.get_name() == "self",
                decl.get_name() == "...",
            )
        {
            return Ok((param_type, ParamInferenceSource::Contextual));
        }

        if let Some(param_type) = find_param_type_from_outer_factory_member(
            db,
            current_member_id,
            param_idx,
            colon_define,
            decl.get_name() == "...",
        ) {
            return Ok((param_type, ParamInferenceSource::Concrete));
        }
    }

    if let Some(file_hint_type) = infer_param_type_from_file_hint(db, decl) {
        return Ok((file_hint_type, ParamInferenceSource::Explicit));
    }

    if let Some(call_site_type) = db
        .get_call_site_param_index()
        .get_inferred_param(&signature_id, param_idx)
    {
        let source = db
            .get_call_site_param_index()
            .get_inferred_param_fact(&signature_id, param_idx)
            .map_or(ParamInferenceSource::Concrete, |fact| {
                if fact.confidence() >= crate::LuaInferenceConfidence::Certain {
                    ParamInferenceSource::Concrete
                } else {
                    ParamInferenceSource::Contextual
                }
            });
        return Ok((call_site_type.clone(), source));
    }

    if let Some(cache) = cache.as_mut()
        && let Some(call_arg_type) =
            infer_param_type_from_call_sites(db, cache, decl, signature_id, param_idx)
    {
        return Ok((call_arg_type, ParamInferenceSource::Concrete));
    }

    if let Some(param_hint_type) = infer_param_type_from_gmod_name_hint(db, decl.get_name()) {
        if let Some(cache) = cache.as_mut()
            && let Some(arg_type) =
                infer_unread_local_call_site_args(db, cache, signature_id, param_idx)?
        {
            return Ok((arg_type, ParamInferenceSource::Contextual));
        }
        return Ok((param_hint_type, ParamInferenceSource::NameFallback));
    }

    Err(InferFailReason::UnResolveDeclType(decl.get_id()))
}

fn infer_param_type_from_call_sites(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    decl: &LuaDecl,
    signature_id: LuaSignatureId,
    param_idx: usize,
) -> Option<LuaType> {
    if decl.get_file_id() != signature_id.get_file_id() {
        return None;
    }

    let root = db
        .get_vfs()
        .get_syntax_tree(&signature_id.get_file_id())?
        .get_red_root();
    let target_decl_id = db
        .get_signature_index()
        .local_func_decl_for(&signature_id)?;
    let call_sites =
        local_function_call_sites(db, cache, signature_id.get_file_id(), &root, target_decl_id);
    infer_param_type_from_local_call_sites_inner(db, cache, call_sites, param_idx, true)
}

fn infer_param_type_from_local_call_sites_inner(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    call_sites: Vec<(FileId, LuaCallExpr)>,
    param_idx: usize,
    allow_param_args: bool,
) -> Option<LuaType> {
    let mut inferred_type: Option<LuaType> = None;
    for (file_id, call_expr) in call_sites {
        let Some(arg) = call_expr
            .get_args_list()
            .and_then(|args| args.get_args().nth(param_idx))
        else {
            continue;
        };
        if !can_call_arg_drive_local_param_inference(db, file_id, &arg, param_idx) {
            continue;
        }
        if !is_local_call_site_arg_shape_supported(&arg) {
            continue;
        }
        let arg_type = if is_global_pairs_for_range_var_arg(db, file_id, &arg) {
            let Ok(arg_type) = infer_expr(db, cache, arg) else {
                continue;
            };
            arg_type
        } else if let Some(arg_param_decl) = param_arg_decl(db, file_id, &arg) {
            if !allow_param_args {
                continue;
            }
            let Some(arg_type) = infer_forwarded_param_arg_type(db, cache, arg_param_decl) else {
                continue;
            };
            arg_type
        } else {
            let Some(arg_type) = infer_supported_non_param_call_arg_type(db, cache, file_id, arg)
            else {
                continue;
            };
            arg_type
        };
        if arg_type.is_unknown() || arg_type.is_never() {
            continue;
        }
        inferred_type = Some(match inferred_type {
            Some(current) => TypeOps::Union.apply(db, &current, &arg_type),
            None => arg_type,
        });
    }

    inferred_type
}

fn infer_supported_non_param_call_arg_type(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    file_id: FileId,
    arg: LuaExpr,
) -> Option<LuaType> {
    match &arg {
        LuaExpr::LiteralExpr(_) => infer_expr(db, cache, arg).ok(),
        LuaExpr::CallExpr(call_expr) if is_zero_arg_call(call_expr) => {
            infer_expr(db, cache, arg).ok()
        }
        LuaExpr::NameExpr(name_expr) => {
            let decl_id = db
                .get_reference_index()
                .get_local_reference(&file_id)
                .and_then(|refs| refs.get_decl_id(&name_expr.get_range()))?;
            let decl = db.get_decl_index().get_decl(&decl_id)?;
            if !matches!(decl.extra, LuaDeclExtra::Local { .. }) {
                return None;
            }
            let root = db.get_vfs().get_syntax_tree(&file_id)?.get_red_root();
            let value_node = decl.get_value_syntax_id()?.to_node_from_root(&root)?;
            let value_expr = LuaExpr::cast(value_node)?;
            match &value_expr {
                LuaExpr::LiteralExpr(_) => {}
                LuaExpr::CallExpr(call_expr) if is_zero_arg_call(call_expr) => {}
                _ => return None,
            }
            infer_expr(db, cache, value_expr).ok()
        }
        _ => None,
    }
}

/// A parameter name hint is a guess for a parameter nothing else describes. Call
/// sites can pass argument shapes local param inference does not read (index
/// expressions, computed calls), and a guess must not outrank one, so read them
/// here rather than name the parameter from a table it never holds.
fn infer_unread_local_call_site_args(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    signature_id: LuaSignatureId,
    param_idx: usize,
) -> Result<Option<LuaType>, InferFailReason> {
    let Some(target_decl_id) = db.get_signature_index().local_func_decl_for(&signature_id) else {
        return Ok(None);
    };
    let Some(root) = db
        .get_vfs()
        .get_syntax_tree(&signature_id.get_file_id())
        .map(|tree| tree.get_red_root())
    else {
        return Ok(None);
    };

    let unread_args =
        local_function_call_sites(db, cache, signature_id.get_file_id(), &root, target_decl_id)
            .into_iter()
            .filter_map(|(_, call_expr)| {
                call_expr
                    .get_args_list()
                    .and_then(|args| args.get_args().nth(param_idx))
            })
            .filter(|arg| !is_local_call_site_arg_shape_supported(arg));

    let mut inferred_type: Option<LuaType> = None;
    for arg in unread_args {
        let arg_type = match infer_expr(db, cache, arg) {
            Ok(arg_type) => arg_type,
            Err(reason) if reason.is_need_resolve() => return Err(reason),
            Err(_) => continue,
        };
        // An argument keeps only the first value of a multi-return call.
        let arg_type = match &arg_type {
            LuaType::Variadic(multi) => match multi.get_type(0) {
                Some(first) => first.clone(),
                None => continue,
            },
            _ => arg_type,
        };
        if arg_type.is_unknown() || arg_type.is_never() || arg_type.is_any() {
            continue;
        }
        inferred_type = Some(match inferred_type {
            Some(current) => TypeOps::Union.apply(db, &current, &arg_type),
            None => arg_type,
        });
    }

    Ok(inferred_type)
}

fn is_local_call_site_arg_shape_supported(arg: &LuaExpr) -> bool {
    match arg {
        LuaExpr::NameExpr(_) | LuaExpr::LiteralExpr(_) => true,
        LuaExpr::CallExpr(call_expr) => is_zero_arg_call(call_expr),
        _ => false,
    }
}

fn is_zero_arg_call(call_expr: &LuaCallExpr) -> bool {
    call_expr
        .get_args_list()
        .is_none_or(|args| args.get_args().next().is_none())
}

fn infer_forwarded_param_arg_type(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    decl: &LuaDecl,
) -> Option<LuaType> {
    let LuaDeclExtra::Param {
        idx, signature_id, ..
    } = decl.extra
    else {
        return None;
    };
    if decl.get_file_id() != signature_id.get_file_id() {
        return None;
    }

    let root = db
        .get_vfs()
        .get_syntax_tree(&signature_id.get_file_id())?
        .get_red_root();
    let closure = root
        .descendants()
        .filter_map(LuaClosureExpr::cast)
        .find(|closure| closure.get_position() == signature_id.get_position())?;
    let local_func_name = closure
        .get_parent::<LuaLocalFuncStat>()
        .and_then(|local_func| local_func.get_local_name())?;
    let target_decl_id = LuaDeclId::new(signature_id.get_file_id(), local_func_name.get_position());

    let call_sites =
        local_function_call_sites(db, cache, signature_id.get_file_id(), &root, target_decl_id);
    infer_param_type_from_local_call_sites_inner(db, cache, call_sites, idx, false)
}

fn local_function_call_sites(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    file_id: FileId,
    root: &LuaSyntaxNode,
    target_decl_id: LuaDeclId,
) -> Vec<(FileId, LuaCallExpr)> {
    // Parameter inference asks for the same function's call sites once per
    // parameter index, and each miss walks the tree from the root for every
    // reference. Derive the set once and re-resolve the ids on later calls.
    let syntax_ids = match cache.local_function_call_sites_cache.get(&target_decl_id) {
        Some(cached) => cached.clone(),
        None => {
            let ids = Arc::new(find_local_function_call_sites(
                db,
                file_id,
                root,
                target_decl_id,
            ));
            cache
                .local_function_call_sites_cache
                .insert(target_decl_id, ids.clone());
            ids
        }
    };

    syntax_ids
        .iter()
        .filter_map(|syntax_id| {
            let node = syntax_id.to_node_from_root(root)?;
            Some((file_id, LuaCallExpr::cast(node)?))
        })
        .collect()
}

fn find_local_function_call_sites(
    db: &DbIndex,
    file_id: FileId,
    root: &LuaSyntaxNode,
    target_decl_id: LuaDeclId,
) -> Vec<LuaSyntaxId> {
    let Some(decl_refs) = db
        .get_reference_index()
        .get_local_reference(&file_id)
        .and_then(|file_refs| file_refs.get_decl_references(&target_decl_id))
    else {
        return Vec::new();
    };

    let mut cells = decl_refs.cells.clone();
    cells.sort_by_key(|cell| cell.range.start());

    cells
        .into_iter()
        .filter_map(|cell| {
            root.covering_element(cell.range)
                .ancestors()
                .find_map(LuaNameExpr::cast)
                .filter(|name_expr| name_expr.get_range() == cell.range)
        })
        .filter_map(|name_expr| name_expr.get_parent::<LuaCallExpr>())
        .filter(|call_expr| matches!(call_expr.get_prefix_expr(), Some(LuaExpr::NameExpr(_))))
        .map(|call_expr| call_expr.get_syntax_id())
        .collect()
}

fn can_call_arg_drive_local_param_inference(
    db: &DbIndex,
    file_id: FileId,
    arg: &LuaExpr,
    target_param_idx: usize,
) -> bool {
    if is_for_range_var_arg(db, file_id, arg) {
        return is_global_pairs_for_range_var_arg(db, file_id, arg);
    }

    if is_mutable_local_name_arg(db, file_id, arg) {
        return false;
    }

    param_arg_decl(db, file_id, arg).is_none_or(
        |decl| matches!(decl.extra, LuaDeclExtra::Param { idx, .. } if idx == target_param_idx),
    )
}

fn is_mutable_local_name_arg(db: &DbIndex, file_id: FileId, arg: &LuaExpr) -> bool {
    let LuaExpr::NameExpr(name_expr) = arg else {
        return false;
    };
    let Some(file_refs) = db.get_reference_index().get_local_reference(&file_id) else {
        return false;
    };
    let Some(decl_id) = file_refs.get_decl_id(&name_expr.get_range()) else {
        return false;
    };
    file_refs
        .get_decl_references(&decl_id)
        .is_some_and(|decl_refs| decl_refs.mutable)
}

fn param_arg_decl<'a>(db: &'a DbIndex, file_id: FileId, arg: &LuaExpr) -> Option<&'a LuaDecl> {
    let LuaExpr::NameExpr(name_expr) = arg else {
        return None;
    };
    let decl_id = db
        .get_reference_index()
        .get_local_reference(&file_id)
        .and_then(|refs| refs.get_decl_id(&name_expr.get_range()))?;
    let decl = db.get_decl_index().get_decl(&decl_id)?;
    matches!(decl.extra, LuaDeclExtra::Param { .. }).then_some(decl)
}

fn is_for_range_var_arg(db: &DbIndex, file_id: FileId, arg: &LuaExpr) -> bool {
    let LuaExpr::NameExpr(name_expr) = arg else {
        return false;
    };
    let Some(decl_id) = db
        .get_reference_index()
        .get_local_reference(&file_id)
        .and_then(|refs| refs.get_decl_id(&name_expr.get_range()))
    else {
        return false;
    };

    name_expr
        .syntax()
        .ancestors()
        .find_map(LuaForRangeStat::cast)
        .is_some_and(|for_range| {
            for_range
                .get_var_name_list()
                .any(|var_name| var_name.get_position() == decl_id.position)
        })
}

fn is_global_pairs_for_range_var_arg(db: &DbIndex, file_id: FileId, arg: &LuaExpr) -> bool {
    let LuaExpr::NameExpr(name_expr) = arg else {
        return false;
    };
    let Some(decl_id) = db
        .get_reference_index()
        .get_local_reference(&file_id)
        .and_then(|refs| refs.get_decl_id(&name_expr.get_range()))
    else {
        return false;
    };

    name_expr
        .syntax()
        .ancestors()
        .find_map(LuaForRangeStat::cast)
        .is_some_and(|for_range| {
            let is_loop_var = for_range
                .get_var_name_list()
                .any(|var_name| var_name.get_position() == decl_id.position);
            is_loop_var && is_global_pairs_for_range(db, file_id, &for_range)
        })
}

fn is_global_pairs_for_range(db: &DbIndex, file_id: FileId, for_range: &LuaForRangeStat) -> bool {
    let Some(LuaExpr::CallExpr(call_expr)) = for_range.get_expr_list().next() else {
        return false;
    };
    let Some(LuaExpr::NameExpr(name_expr)) = call_expr.get_prefix_expr() else {
        return false;
    };
    if name_expr.get_name_text().as_deref() != Some("pairs") {
        return false;
    }

    db.get_reference_index()
        .get_local_reference(&file_id)
        .and_then(|file_ref| file_ref.get_decl_id(&name_expr.get_range()))
        .is_none()
}

fn find_param_type_from_sibling_members(
    db: &DbIndex,
    current_member_id: LuaMemberId,
    param_idx: usize,
    colon_define: bool,
    is_dots: bool,
    current_type: Option<&LuaType>,
) -> Option<LuaType> {
    if let Some(current_type) = current_type
        && !current_type.is_number()
    {
        return None;
    }

    let member_index = db.get_member_index();
    let owner = member_index.get_current_owner(&current_member_id)?;
    let key = member_index.get_member(&current_member_id)?.get_key();

    let mut final_type = None;
    for member in member_index.get_current_owner_members_for_key(owner, key) {
        if member.get_id() == current_member_id {
            continue;
        }

        let member_type = db
            .get_type_index()
            .get_type_cache(&member.get_id().into())?
            .as_type()
            .clone();
        let Some(param_type) =
            find_overload_param_type_from_type(db, member_type, param_idx, colon_define, is_dots)
        else {
            continue;
        };

        if is_dots && param_type.is_any() {
            return Some(param_type);
        }

        final_type = match final_type {
            Some(existing) => Some(TypeOps::Union.apply(db, &existing, &param_type)),
            None => Some(param_type),
        };
    }

    final_type
}

type InheritedParamKey = (LuaMemberId, usize, bool, bool, FileId, TextSize);

thread_local! {
    /// Memo for [`find_param_type_from_inherited_members`], paired with the
    /// `type_structure_revision` it was built against.
    ///
    /// Thread-local rather than a field on `DbIndex` because `&DbIndex` is
    /// shared across worker threads, so the memo cannot live behind a `RefCell`
    /// on the struct without giving up `Sync`.
    static INHERITED_PARAM_MEMO: std::cell::RefCell<(u64, rustc_hash::FxHashMap<InheritedParamKey, Option<LuaType>>)> =
        std::cell::RefCell::new((u64::MAX, rustc_hash::FxHashMap::default()));
}

/// The parameter's type as declared by an inherited member, if any.
///
/// This is the single most expensive step of parameter inference: the
/// unresolve pipeline's reachability probe calls it once per deferred
/// parameter, and on the CityRP benchmark it accounted for ~0.29s of a 2.29s
/// edit — almost entirely in the visibility-aware member lookup it performs per
/// super type.
///
/// The same key is asked repeatedly across the retry loop's iterations, so the
/// answer is memoized against `type_structure_revision`: any mutable access to
/// the type or member index discards the memo, which makes a stale answer
/// impossible even though the loop mutates the db as it resolves.
fn find_param_type_from_inherited_members(
    db: &DbIndex,
    current_member_id: LuaMemberId,
    param_idx: usize,
    colon_define: bool,
    is_dots: bool,
    caller_file_id: FileId,
    caller_position: TextSize,
) -> Option<LuaType> {
    let revision = db.type_structure_revision();
    let key = (
        current_member_id,
        param_idx,
        colon_define,
        is_dots,
        caller_file_id,
        caller_position,
    );

    let cached = INHERITED_PARAM_MEMO.with(|memo| {
        let mut memo = memo.borrow_mut();
        if memo.0 != revision {
            memo.0 = revision;
            memo.1.clear();
            return None;
        }
        memo.1.get(&key).cloned()
    });
    if let Some(cached) = cached {
        return cached;
    }

    let found = find_param_type_from_inherited_members_uncached(
        db,
        current_member_id,
        param_idx,
        colon_define,
        is_dots,
        caller_file_id,
        caller_position,
    );

    INHERITED_PARAM_MEMO.with(|memo| {
        let mut memo = memo.borrow_mut();
        // Only store if nothing bumped the revision while we were computing.
        if memo.0 == revision {
            memo.1.insert(key, found.clone());
        }
    });
    found
}

fn find_param_type_from_inherited_members_uncached(
    db: &DbIndex,
    current_member_id: LuaMemberId,
    param_idx: usize,
    colon_define: bool,
    is_dots: bool,
    caller_file_id: FileId,
    caller_position: TextSize,
) -> Option<LuaType> {
    let member_index = db.get_member_index();
    let owner = member_index.get_current_owner(&current_member_id)?;
    let owner_id = owner.get_type_id()?;
    let key = member_index
        .get_member(&current_member_id)?
        .get_key()
        .clone();
    let caller_workspace_id = db.get_module_index().get_workspace_id(caller_file_id);
    let super_types = match caller_workspace_id {
        Some(workspace_id) => visible_super_types_in_workspace_for_file_at_offset(
            db,
            owner_id,
            workspace_id,
            caller_file_id,
            caller_position,
        ),
        None => db.get_type_index().get_super_types(owner_id),
    }?;

    for super_type in super_types {
        let member_infos = match caller_workspace_id {
            Some(workspace_id) => find_members_with_key_in_workspace_for_file_at_offset(
                db,
                &super_type,
                key.clone(),
                false,
                workspace_id,
                caller_file_id,
                caller_position,
            ),
            None => find_members_with_key(db, &super_type, key.clone(), false),
        };
        let Some(member_infos) = member_infos else {
            continue;
        };

        for member_info in member_infos {
            if let Some(param_type) =
                find_param_type_from_type(db, member_info.typ, param_idx, colon_define, is_dots)
            {
                return Some(param_type);
            }
        }
    }

    None
}

fn find_param_type_from_outer_factory_member(
    db: &DbIndex,
    member_id: LuaMemberId,
    param_idx: usize,
    colon_define: bool,
    is_dots: bool,
) -> Option<LuaType> {
    let root = db
        .get_vfs()
        .get_syntax_tree(&member_id.file_id)?
        .get_red_root();
    let current_node = member_id.get_syntax_id().to_node_from_root(&root)?;
    let outer_func = current_node
        .ancestors()
        .filter_map(LuaFuncStat::cast)
        .nth(1)?;
    if !outer_factory_returns_current_member_receiver(&current_node, &outer_func) {
        return None;
    }

    let outer_func_name = outer_func.get_func_name()?;
    let outer_name = outer_func_name.get_access_path()?;
    let member = db.get_member_index().get_member(&member_id)?;
    let member_key = member.get_key().clone();

    let owner_types = [
        LuaType::Ref(LuaTypeDeclId::global(&outer_name)),
        LuaType::Def(LuaTypeDeclId::global(&outer_name)),
        LuaType::Namespace(smol_str::SmolStr::new(&outer_name).into()),
    ];
    for owner_type in owner_types {
        let Some(member_infos) = find_members_with_key(db, &owner_type, member_key.clone(), true)
        else {
            continue;
        };

        let informative_types: Vec<_> = member_infos
            .into_iter()
            .filter(|info| info.feature.is_some_and(|feature| feature.is_meta_decl()))
            .filter_map(|info| {
                find_param_type_from_type(db, info.typ, param_idx, colon_define, is_dots)
            })
            .filter(|typ| {
                !is_broad_factory_param_type(typ) && is_structured_factory_param_type(typ)
            })
            .collect();
        if informative_types.is_empty() {
            continue;
        }

        return merge_factory_param_candidates(db, informative_types, is_dots);
    }

    None
}

fn merge_factory_param_candidates(
    db: &DbIndex,
    informative_types: Vec<LuaType>,
    is_dots: bool,
) -> Option<LuaType> {
    let mut final_type = None;
    for param_type in informative_types {
        if is_dots && param_type.is_any() {
            return Some(param_type);
        }

        final_type = match final_type {
            Some(existing) => Some(TypeOps::Union.apply(db, &existing, &param_type)),
            None => Some(param_type),
        };
    }

    final_type
}

fn is_broad_factory_param_type(typ: &LuaType) -> bool {
    typ.is_any() || typ.is_unknown() || typ.is_nil() || typ.is_table()
}

fn is_structured_factory_param_type(typ: &LuaType) -> bool {
    match typ {
        LuaType::Ref(_) | LuaType::Def(_) | LuaType::Object(_) | LuaType::TableGeneric(_) => true,
        LuaType::Union(union) => union.types().any(is_structured_factory_param_type),
        LuaType::Intersection(intersection) => intersection
            .get_types()
            .iter()
            .any(is_structured_factory_param_type),
        LuaType::TableOf(inner) => is_structured_factory_param_type(inner),
        LuaType::TypeGuard(inner) => is_structured_factory_param_type(inner),
        _ => false,
    }
}

fn outer_factory_returns_current_member_receiver(
    current_node: &rowan::SyntaxNode<glua_parser::LuaLanguage>,
    outer_func: &LuaFuncStat,
) -> bool {
    let Some(current_func) = current_node.ancestors().find_map(LuaFuncStat::cast) else {
        return false;
    };
    let Some(receiver_name) = member_receiver_name(&current_func) else {
        return false;
    };
    if !outer_factory_declares_direct_local(outer_func, receiver_name.as_str()) {
        return false;
    }

    outer_func
        .descendants::<LuaReturnStat>()
        .any(|return_stat| {
            let is_direct_outer_return = return_stat
                .syntax()
                .ancestors()
                .find_map(LuaFuncStat::cast)
                .is_some_and(|func| func.get_syntax_id() == outer_func.get_syntax_id());
            is_direct_outer_return
                && return_stat.get_expr_list().any(|expr| match expr {
                    LuaExpr::NameExpr(name_expr) => name_expr
                        .get_name_text()
                        .is_some_and(|name| name == receiver_name),
                    _ => false,
                })
        })
}

fn outer_factory_declares_direct_local(outer_func: &LuaFuncStat, receiver_name: &str) -> bool {
    outer_func.descendants::<LuaLocalStat>().any(|local_stat| {
        let is_direct_outer_local = local_stat
            .syntax()
            .ancestors()
            .find_map(LuaFuncStat::cast)
            .is_some_and(|func| func.get_syntax_id() == outer_func.get_syntax_id());
        is_direct_outer_local
            && local_stat
                .get_local_name_list()
                .any(|local_name| local_name.get_text() == receiver_name)
    })
}

fn member_receiver_name(func: &LuaFuncStat) -> Option<smol_str::SmolStr> {
    let LuaVarExpr::IndexExpr(index_expr) = func.get_func_name()? else {
        return None;
    };
    let LuaExpr::NameExpr(name_expr) = index_expr.get_prefix_expr()? else {
        return None;
    };
    name_expr.get_name_text()
}

fn find_overload_param_type_from_type(
    db: &DbIndex,
    source_type: LuaType,
    param_idx: usize,
    current_colon_define: bool,
    is_dots: bool,
) -> Option<LuaType> {
    match source_type {
        LuaType::Signature(signature_id) => {
            let signature = db.get_signature_index().get(&signature_id)?;
            find_signature_overload_param_type(
                db,
                signature,
                param_idx,
                current_colon_define,
                is_dots,
            )
        }
        LuaType::Union(union_types) => {
            let mut final_type = None;
            for ty in union_types.into_vec() {
                let Some(param_type) = find_overload_param_type_from_type(
                    db,
                    ty,
                    param_idx,
                    current_colon_define,
                    is_dots,
                ) else {
                    continue;
                };

                if is_dots && param_type.is_any() {
                    return Some(param_type);
                }

                final_type = match final_type {
                    Some(existing) => Some(TypeOps::Union.apply(db, &existing, &param_type)),
                    None => Some(param_type),
                };
            }
            final_type
        }
        _ => None,
    }
}

pub fn infer_param_with_cache(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    decl: &LuaDecl,
) -> InferResult {
    let decl_id = decl.get_id();
    if let Some(cache_entry) = cache.param_type_cache.get(&decl_id) {
        return match cache_entry {
            CacheEntry::Cache(typ) => Ok(typ.clone()),
            CacheEntry::Error(reason) => Err(reason.clone()),
            CacheEntry::Ready => Err(InferFailReason::RecursiveInfer),
        };
    }

    cache.param_type_cache.insert(decl_id, CacheEntry::Ready);
    let result = infer_param_inner(db, Some(cache), decl);
    match &result {
        Ok((typ, source)) => {
            cache
                .param_type_cache
                .insert(decl_id, CacheEntry::Cache(typ.clone()));
            cache
                .param_type_source_cache
                .insert(decl_id, CacheEntry::Cache(*source));
        }
        Err(reason) if cache.get_config().analysis_phase.is_diagnostics() => {
            cache
                .param_type_cache
                .insert(decl_id, CacheEntry::Error(reason.clone()));
            cache
                .param_type_source_cache
                .insert(decl_id, CacheEntry::Error(reason.clone()));
        }
        Err(_) => {
            cache.param_type_cache.remove(&decl_id);
            cache.param_type_source_cache.remove(&decl_id);
        }
    }

    result.map(|(typ, _)| typ)
}

pub(crate) fn infer_param_is_weak(db: &DbIndex, cache: &mut LuaInferCache, decl: &LuaDecl) -> bool {
    let _ = infer_param_with_cache(db, cache, decl);
    matches!(
        cache.param_type_source_cache.get(&decl.get_id()),
        Some(CacheEntry::Cache(source)) if source.is_weak()
    )
}

fn direct_table_field_from_member_id(
    root: &LuaChunk,
    member_id: LuaMemberId,
) -> Option<LuaTableField> {
    // Reconstruct the exact table field by syntax id instead of scanning every table field.
    member_id
        .get_syntax_id()
        .to_node_from_root(root.syntax())
        .and_then(LuaTableField::cast)
}

fn find_param_type_from_contextual_member(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    member_id: LuaMemberId,
    param_idx: usize,
    colon_define: bool,
    is_self_param: bool,
    is_dots: bool,
) -> Option<LuaType> {
    let root = db
        .get_vfs()
        .get_syntax_tree(&member_id.file_id)?
        .get_chunk_node();
    let table_field = direct_table_field_from_member_id(&root, member_id)?;
    let field_type = infer_table_field_value_should_be(db, cache, table_field.clone()).ok()?;
    find_param_type_from_type(db, field_type, param_idx, colon_define, is_dots).or_else(|| {
        if param_idx != 0 || colon_define || is_dots || !is_self_param {
            return None;
        }

        let parent_table = table_field.get_parent::<LuaTableExpr>()?;
        let parent_type = infer_table_should_be(db, cache, parent_table).ok()?;
        if is_vgui_panel_context_type(db, &parent_type) {
            Some(parent_type)
        } else {
            None
        }
    })
}

fn is_vgui_panel_context_type(db: &DbIndex, typ: &LuaType) -> bool {
    match typ {
        LuaType::Def(type_id) | LuaType::Ref(type_id) => type_decl_is_vgui_panel(db, type_id, 0),
        _ => false,
    }
}

pub(crate) fn type_decl_is_vgui_panel(db: &DbIndex, type_id: &LuaTypeDeclId, depth: usize) -> bool {
    if depth > 8 {
        return false;
    }

    if type_id.get_name() == "Panel" {
        return true;
    }

    if db
        .get_type_index()
        .get_type_decl(type_id)
        .is_some_and(|decl| decl.is_auto_generated())
    {
        return true;
    }

    db.get_type_index()
        .get_super_types_iter(type_id)
        .is_some_and(|mut super_types| {
            super_types.any(|super_type| match super_type {
                LuaType::Def(super_id) | LuaType::Ref(super_id) => {
                    type_decl_is_vgui_panel(db, super_id, depth + 1)
                }
                _ => false,
            })
        })
}

fn infer_param_type_from_gmod_name_hint(db: &DbIndex, param_name: &str) -> Option<LuaType> {
    let hints = &db.get_emmyrc().gmod.file_param_defaults;
    if hints.is_empty() {
        return None;
    }

    let lowercase_name = param_name.to_ascii_lowercase();
    let hint = hints
        .get(param_name)
        .or_else(|| hints.get(&lowercase_name))?
        .trim();
    if hint.is_empty() {
        return None;
    }

    resolve_param_hint_type(db, hint)
}

fn infer_param_type_from_file_hint(db: &DbIndex, decl: &LuaDecl) -> Option<LuaType> {
    let target_name = decl.get_name().to_ascii_lowercase();
    let type_text = db
        .get_gmod_infer_index()
        .get_file_param_type_text(&decl.get_file_id(), &target_name)?;
    resolve_param_hint_type(db, type_text)
}

fn resolve_param_hint_type(db: &DbIndex, hint: &str) -> Option<LuaType> {
    let normalized_hint = hint.trim();
    let (type_name, nullable) = if let Some(name) = normalized_hint.strip_suffix('?') {
        (name.trim(), true)
    } else {
        (normalized_hint, false)
    };

    if type_name.is_empty() {
        return None;
    }

    let mut resolved_type = match type_name {
        "any" => LuaType::Any,
        "unknown" => LuaType::Unknown,
        "string" => LuaType::String,
        "number" => LuaType::Number,
        "integer" => LuaType::Integer,
        "boolean" => LuaType::Boolean,
        "table" => LuaType::Table,
        "function" => LuaType::Function,
        "thread" => LuaType::Thread,
        "userdata" => LuaType::Userdata,
        "nil" => LuaType::Nil,
        _ => {
            let type_decl_id = LuaTypeDeclId::global(type_name);
            db.get_type_index().get_type_decl(&type_decl_id)?;
            LuaType::Ref(type_decl_id)
        }
    };

    if nullable && !resolved_type.is_nullable() {
        resolved_type = TypeOps::Union.apply(db, &resolved_type, &LuaType::Nil);
    }

    Some(resolved_type)
}

pub fn find_decl_member_type(db: &DbIndex, member_id: LuaMemberId) -> InferResult {
    let item = db
        .get_member_index()
        .get_member_item_by_member_id(member_id)
        .ok_or(InferFailReason::None)?;
    item.resolve_type(db)
}

fn adjust_param_idx(
    param_idx: usize,
    current_colon_define: bool,
    decl_colon_defined: bool,
) -> usize {
    let mut adjusted_idx = param_idx;
    match (current_colon_define, decl_colon_defined) {
        (true, false) => {
            adjusted_idx += 1;
        }
        (false, true) => adjusted_idx = adjusted_idx.saturating_sub(1),
        _ => {}
    }
    adjusted_idx
}

fn check_dots_param_types(
    params: &[(String, Option<LuaType>)],
    param_idx: usize,
    cur_type: &Option<LuaType>,
) -> Option<LuaType> {
    for (_, typ) in params.iter().skip(param_idx) {
        if let Some(typ) = typ
            && let Some(cur_type) = cur_type
            && cur_type != typ
        {
            return Some(LuaType::Any);
        }
    }
    None
}

fn union_signature_overload_param_types(
    db: &DbIndex,
    signature: &LuaSignature,
    mut final_type: LuaType,
    param_idx: usize,
    current_colon_define: bool,
    is_dots: bool,
) -> LuaType {
    if let Some(overload_type) =
        find_signature_overload_param_type(db, signature, param_idx, current_colon_define, is_dots)
    {
        final_type = TypeOps::Union.apply(db, &final_type, &overload_type);
    }

    final_type
}

fn find_signature_overload_param_type(
    db: &DbIndex,
    signature: &LuaSignature,
    param_idx: usize,
    current_colon_define: bool,
    is_dots: bool,
) -> Option<LuaType> {
    let mut final_type = None;
    for overload in &signature.overloads {
        let adjusted_idx =
            adjust_param_idx(param_idx, current_colon_define, overload.is_colon_define());

        let Some((_, cur_type)) = overload.get_params().get(adjusted_idx) else {
            continue;
        };

        if is_dots
            && let Some(any_type) =
                check_dots_param_types(overload.get_params(), adjusted_idx, cur_type)
        {
            return Some(any_type);
        }

        if let Some(typ) = cur_type {
            final_type = match final_type {
                Some(existing) => Some(TypeOps::Union.apply(db, &existing, typ)),
                None => Some(typ.clone()),
            };
        }
    }

    final_type
}

fn find_param_type_from_type(
    db: &DbIndex,
    source_type: LuaType,
    param_idx: usize,
    current_colon_define: bool,
    is_dots: bool,
) -> Option<LuaType> {
    match source_type {
        LuaType::Signature(signature_id) => {
            let signature = db.get_signature_index().get(&signature_id)?;
            let adjusted_idx =
                adjust_param_idx(param_idx, current_colon_define, signature.is_colon_define);

            match signature.get_param_info_by_id(adjusted_idx) {
                Some(param_info) => {
                    let mut typ = param_info.type_ref.clone();
                    if param_info.nullable && !typ.is_nullable() {
                        typ = TypeOps::Union.apply(db, &typ, &LuaType::Nil);
                    }
                    Some(union_signature_overload_param_types(
                        db,
                        signature,
                        typ,
                        param_idx,
                        current_colon_define,
                        is_dots,
                    ))
                }
                None => {
                    if !signature.param_docs.is_empty() {
                        return None;
                    }

                    let mut final_type = None;
                    for overload in &signature.overloads {
                        let adjusted_idx = adjust_param_idx(
                            param_idx,
                            current_colon_define,
                            overload.is_colon_define(),
                        );

                        let cur_type =
                            if let Some((_, typ)) = overload.get_params().get(adjusted_idx) {
                                typ.clone()
                            } else {
                                return None;
                            };

                        if is_dots
                            && let Some(any_type) = check_dots_param_types(
                                overload.get_params(),
                                adjusted_idx,
                                &cur_type,
                            )
                        {
                            return Some(any_type);
                        }

                        if let Some(typ) = cur_type {
                            final_type = match final_type {
                                Some(existing) => Some(TypeOps::Union.apply(db, &existing, &typ)),
                                None => Some(typ.clone()),
                            };
                        }
                    }
                    final_type
                }
            }
        }
        LuaType::DocFunction(f) => {
            let adjusted_idx =
                adjust_param_idx(param_idx, current_colon_define, f.is_colon_define());
            if let Some((_, typ)) = f.get_params().get(adjusted_idx) {
                let cur_type = typ.clone();
                if is_dots
                    && let Some(any_type) =
                        check_dots_param_types(f.get_params(), adjusted_idx, &cur_type)
                {
                    return Some(any_type);
                }
                cur_type
            } else {
                None
            }
        }
        LuaType::Union(_) => {
            find_param_type_from_union(db, source_type, param_idx, current_colon_define, is_dots)
        }
        _ => None,
    }
}

fn find_param_type_from_union(
    db: &DbIndex,
    source_type: LuaType,
    param_idx: usize,
    origin_colon_define: bool,
    is_dots: bool,
) -> Option<LuaType> {
    match source_type {
        LuaType::Signature(signature_id) => {
            let signature = db.get_signature_index().get(&signature_id)?;
            if !signature.param_docs.is_empty() {
                let adjusted_idx =
                    adjust_param_idx(param_idx, origin_colon_define, signature.is_colon_define);
                return if let Some(param_info) = signature.get_param_info_by_id(adjusted_idx) {
                    let mut typ = param_info.type_ref.clone();
                    if param_info.nullable && !typ.is_nullable() {
                        typ = TypeOps::Union.apply(db, &typ, &LuaType::Nil);
                    }
                    Some(union_signature_overload_param_types(
                        db,
                        signature,
                        typ,
                        param_idx,
                        origin_colon_define,
                        is_dots,
                    ))
                } else {
                    None
                };
            }
            let mut final_type = None;
            for overload in &signature.overloads {
                let adjusted_idx =
                    adjust_param_idx(param_idx, origin_colon_define, overload.is_colon_define());

                let cur_type = if let Some((_, typ)) = overload.get_params().get(adjusted_idx) {
                    typ.clone()
                } else {
                    return None;
                };

                if is_dots
                    && let Some(any_type) =
                        check_dots_param_types(overload.get_params(), adjusted_idx, &cur_type)
                {
                    return Some(any_type);
                }

                if let Some(typ) = cur_type {
                    final_type = match final_type {
                        Some(existing) => Some(TypeOps::Union.apply(db, &existing, &typ)),
                        None => Some(typ.clone()),
                    };
                }
            }
            final_type
        }
        LuaType::DocFunction(f) => {
            let adjusted_idx =
                adjust_param_idx(param_idx, origin_colon_define, f.is_colon_define());
            let cur_type = if let Some((_, typ)) = f.get_params().get(adjusted_idx) {
                typ.clone()
            } else {
                return None;
            };

            if is_dots
                && let Some(any_type) =
                    check_dots_param_types(f.get_params(), adjusted_idx, &cur_type)
            {
                return Some(any_type);
            }

            cur_type
        }
        LuaType::Union(union_types) => {
            let mut final_type = None;
            for ty in union_types.into_vec() {
                if let Some(ty) = find_param_type_from_union(
                    db,
                    ty.clone(),
                    param_idx,
                    origin_colon_define,
                    is_dots,
                ) {
                    if is_dots && ty.is_any() {
                        return Some(ty);
                    }
                    final_type = match final_type {
                        Some(existing) => Some(TypeOps::Union.apply(db, &existing, &ty)),
                        None => Some(ty),
                    };
                }
            }
            final_type
        }
        _ => None,
    }
}

/// `local x = foo` runs before any `function foo` / `foo = ...` statement
/// that appears later in the same file, so those declarations cannot be the
/// value it captures. Without this, the classic wrapper idiom (`local orig
/// = foo` + `function foo(...) return orig(...) end`) binds `orig` to the
/// wrapper itself, making it self-recursive and collapsing its return type.
fn reads_global_declared_later_in_file(
    db: &DbIndex,
    file_id: FileId,
    name_expr: &LuaNameExpr,
    name: &str,
) -> bool {
    if name_expr.get_parent::<LuaLocalStat>().is_none() {
        return false;
    }
    if name_expr
        .syntax()
        .ancestors()
        .any(|ancestor| LuaClosureExpr::can_cast(ancestor.kind().into()))
    {
        return false;
    }

    let position = name_expr.get_position();
    db.get_global_index()
        .get_global_decl_ids(name)
        .is_some_and(|decl_ids| {
            decl_ids
                .iter()
                .any(|id| id.file_id == file_id && id.position > position)
        })
}

pub fn infer_global_type(
    db: &DbIndex,
    current_file_id: Option<FileId>,
    call_offset: Option<TextSize>,
    name: &str,
) -> InferResult {
    infer_global_type_impl(db, current_file_id, call_offset, name, false)
}

fn infer_global_type_impl(
    db: &DbIndex,
    current_file_id: Option<FileId>,
    call_offset: Option<TextSize>,
    name: &str,
    skip_later_decls_in_current_file: bool,
) -> InferResult {
    if db.get_emmyrc().gmod.enabled && name == "NULL" {
        let null_decl_id = LuaTypeDeclId::global("NULL");
        if db.get_type_index().get_type_decl(&null_decl_id).is_some() {
            return Ok(LuaType::Ref(null_decl_id));
        }
    }

    if let Some(module_decl_type) =
        infer_legacy_module_local_type(db, current_file_id, call_offset, name)
    {
        return Ok(module_decl_type);
    }

    // A name matching a legacy module path resolves to that module's namespace,
    // even if a synthetic global decl exists for it (we add one so goto-def
    // jumps to the `module(...)` call site). Member access like `tc.foo` then
    // routes through the namespace as expected.
    if has_legacy_module_namespace_for_file(db, current_file_id, name) {
        return Ok(LuaType::Namespace(smol_str::SmolStr::new(name).into()));
    }

    let module_index = db.get_module_index();
    let global_index = db.get_global_index();
    let priority_tiers = if let Some(current_workspace_id) =
        current_file_id.and_then(|file_id| module_index.get_workspace_id(file_id))
    {
        match global_index.get_global_decl_id_priority_tiers(
            name,
            module_index,
            current_workspace_id,
        ) {
            Some(tiers) => tiers,
            None => {
                return Err(InferFailReason::None);
            }
        }
    } else {
        vec![match global_index.get_global_decl_ids(name).cloned() {
            Some(decls) => (0, decls),
            None => {
                return Err(InferFailReason::None);
            }
        }]
    };

    if priority_tiers.is_empty() {
        return Err(InferFailReason::None);
    }

    // Drop declarations that the reference cannot have seen yet (see
    // `reads_global_declared_later_in_file`). If nothing is left, no earlier
    // declaration exists at all and the normal resolution stands.
    let mut priority_tiers = priority_tiers;
    if skip_later_decls_in_current_file
        && let Some((file_id, offset)) = current_file_id.zip(call_offset)
    {
        let filtered: Vec<_> = priority_tiers
            .iter()
            .filter_map(|(priority, decl_ids)| {
                let decl_ids: Vec<_> = decl_ids
                    .iter()
                    .copied()
                    .filter(|id| !(id.file_id == file_id && id.position > offset))
                    .collect();
                (!decl_ids.is_empty()).then_some((*priority, decl_ids))
            })
            .collect();
        if !filtered.is_empty() {
            priority_tiers = filtered;
        }
    }

    // A top-priority global can exist before its type cache is resolved while
    // analyzing assignments such as `x = x`. It must not hide lower-priority
    // declarations that describe the value being read.
    let call_state_mask = if db.get_emmyrc().gmod.enabled {
        current_file_id
            .zip(call_offset)
            .map(|(file_id, call_offset)| {
                db.get_gmod_infer_index()
                    .get_state_mask_at_offset(&file_id, call_offset)
            })
    } else {
        None
    };

    let mut last_resolve_reason = InferFailReason::None;
    let mut saw_compatible_tier = false;
    let mut fallback_best_tier = None;
    let mut nil_fallback_type = None;
    let len = priority_tiers.len();
    for i in 0..len {
        let (_, decl_ids) = &priority_tiers[i];
        let decl_ids = if let Some(call_state_mask) = call_state_mask {
            let selected_decl_ids = select_realm_compatible_decl_ids_for_global_infer_tier(
                db,
                call_state_mask,
                decl_ids,
            );
            if selected_decl_ids.is_empty() {
                if fallback_best_tier.is_none() {
                    fallback_best_tier = Some(decl_ids.clone());
                }
                continue;
            }

            selected_decl_ids
        } else {
            decl_ids.clone()
        };
        if decl_ids.is_empty() {
            continue;
        }
        saw_compatible_tier = true;
        match infer_global_type_from_decl_ids(db, decl_ids) {
            Ok(typ) if typ.is_nil() => {
                nil_fallback_type.get_or_insert(typ);
                continue;
            }
            Ok(typ) => {
                if matches!(typ, LuaType::TableConst(_)) {
                    for (_, next_decl_ids) in priority_tiers.iter().skip(i + 1) {
                        let next_decl_ids = if let Some(call_state_mask) = call_state_mask {
                            select_realm_compatible_decl_ids_for_global_infer_tier(
                                db,
                                call_state_mask,
                                next_decl_ids,
                            )
                        } else {
                            next_decl_ids.clone()
                        };
                        if next_decl_ids.is_empty() {
                            continue;
                        }
                        if let Ok(next_typ) = infer_global_type_from_decl_ids(db, next_decl_ids) {
                            if matches!(next_typ, LuaType::Ref(_) | LuaType::Def(_)) {
                                return Ok(next_typ);
                            }
                        }
                    }
                }
                return Ok(typ);
            }
            Err(reason) if can_fall_through_global_tier(&reason) => last_resolve_reason = reason,
            Err(reason) => return Err(reason),
        }
    }

    if !saw_compatible_tier && let Some(decl_ids) = fallback_best_tier {
        match infer_global_type_from_decl_ids(db, decl_ids) {
            Ok(typ) if typ.is_nil() => {
                nil_fallback_type.get_or_insert(typ);
            }
            Ok(typ) => return Ok(typ),
            Err(reason) => last_resolve_reason = reason,
        }
    }

    if let Some(typ) = nil_fallback_type {
        return Ok(typ);
    }

    Err(last_resolve_reason)
}

fn can_fall_through_global_tier(reason: &InferFailReason) -> bool {
    matches!(
        reason,
        InferFailReason::UnResolveDeclType(_)
            | InferFailReason::UnResolveTypeDecl(_)
            | InferFailReason::UnResolveMemberType(_)
    )
}

fn infer_global_type_from_decl_ids(db: &DbIndex, decl_ids: Vec<LuaDeclId>) -> InferResult {
    if decl_ids.is_empty() {
        return Err(InferFailReason::None);
    }

    if decl_ids.len() == 1 {
        let id = decl_ids[0];
        let typ = match db.get_type_index().get_type_cache(&id.into()) {
            Some(type_cache) => type_cache.as_type().clone(),
            None => return Err(InferFailReason::UnResolveDeclType(id)),
        };
        return if typ.contain_tpl() {
            // This decl is located in a generic function,
            // and is type contains references to generic variables
            // of this function.
            Ok(LuaType::Unknown)
        } else {
            Ok(typ)
        };
    }

    let mut sorted_decl_ids = decl_ids;
    sorted_decl_ids.sort_by(|a, b| {
        let a_is_std = db.get_module_index().is_std(&a.file_id);
        let b_is_std = db.get_module_index().is_std(&b.file_id);
        b_is_std.cmp(&a_is_std)
    });

    let mut callable_type: Option<LuaType> = None;
    let mut def_or_ref_type: Option<LuaType> = None;
    let mut table_types = Vec::new();
    let mut last_resolve_reason = InferFailReason::None;
    let mut saw_resolved_decl_type = false;
    let mut saw_nil = false;
    let mut saw_unhandled_non_nil = false;
    for decl_id in sorted_decl_ids {
        let decl_type_cache = db.get_type_index().get_type_cache(&decl_id.into());
        match decl_type_cache {
            Some(type_cache) => {
                saw_resolved_decl_type = true;
                let typ = type_cache.as_type();

                if typ.contain_tpl() {
                    // This decl is located in a generic function,
                    // and is type contains references to generic variables
                    // of this function.
                    continue;
                }

                if typ.is_nil() {
                    saw_nil = true;
                    continue;
                }

                if matches!(typ, LuaType::Signature(_) | LuaType::DocFunction(_))
                    || typ.is_function()
                {
                    callable_type = Some(match callable_type {
                        Some(existing) => TypeOps::Union.apply(db, &existing, typ),
                        None => typ.clone(),
                    });
                    continue;
                }

                if (typ.is_def() || typ.is_ref() || matches!(typ, LuaType::Instance(_)))
                    && def_or_ref_type.is_none()
                {
                    def_or_ref_type = Some(typ.clone());
                    continue;
                }

                if collect_global_table_merge_candidates(typ, &mut table_types) {
                    continue;
                }

                // The type was resolved but did not match any collection
                // branch (e.g. Integer, String, Boolean). Mark it so the
                // all-nil fallback below does not fire incorrectly.
                saw_unhandled_non_nil = true;
            }
            None => {
                last_resolve_reason = InferFailReason::UnResolveDeclType(decl_id);
            }
        }
    }

    if let Some(callable_type) = callable_type {
        return Ok(callable_type);
    }

    if let Some(def_or_ref_type) = def_or_ref_type {
        return Ok(def_or_ref_type);
    }

    if !table_types.is_empty() {
        return Ok(merge_open_table_types(db, table_types));
    }

    if saw_nil && !saw_unhandled_non_nil {
        // Every resolved, non-template decl in this tier was nil. Return
        // Ok(Nil) so the caller can use it as a nil_fallback_type and still
        // consult lower-priority tiers (e.g. a library DocType annotation).
        return Ok(LuaType::Nil);
    }

    if saw_resolved_decl_type {
        Err(InferFailReason::None)
    } else {
        Err(last_resolve_reason)
    }
}

fn collect_global_table_merge_candidates(typ: &LuaType, table_types: &mut Vec<LuaType>) -> bool {
    match typ {
        LuaType::Object(_) => {
            table_types.push(typ.clone());
            true
        }
        LuaType::Union(union) => {
            let mut nested = Vec::new();
            if union
                .types()
                .all(|typ| collect_global_table_merge_candidates(typ, &mut nested))
            {
                table_types.extend(nested);
                true
            } else {
                false
            }
        }
        _ if typ.is_table() => {
            table_types.push(typ.clone());
            true
        }
        _ => false,
    }
}

fn infer_legacy_module_local_type(
    db: &DbIndex,
    current_file_id: Option<FileId>,
    call_offset: Option<TextSize>,
    name: &str,
) -> Option<LuaType> {
    let file_id = current_file_id?;
    let position = call_offset?;
    let module_env = db
        .get_module_index()
        .get_legacy_module_env_at(file_id, position)?;

    let decl_tree = db.get_decl_index().get_decl_tree(&file_id)?;
    if let Some(decl) = decl_tree
        .find_local_decl(name, position)
        .filter(|decl| {
            decl.is_module_scoped()
                && decl.get_module_path() == Some(module_env.module_path.as_str())
        })
        .or_else(|| {
            decl_tree.find_module_scoped_decl_anywhere(
                name,
                &module_env.module_path,
                module_env.activation_position,
            )
        })
    {
        return db
            .get_type_index()
            .get_type_cache(&decl.get_id().into())
            .map(|cache| cache.as_type().clone());
    }

    // fallback: cross-file member search via GlobalPath member index
    // (module-scoped decls in other files are stored under GlobalPath, not the global index)
    let owner = LuaMemberOwner::GlobalPath(crate::GlobalId::new(&module_env.module_path));
    let member_key = LuaMemberKey::Name(name.into());
    if let Some(member_item) = db.get_member_index().get_member_item(&owner, &member_key) {
        if let Ok(ty) = member_item.resolve_type_with_realm(db, &file_id) {
            return Some(ty);
        }
        // For module-scoped function/value decls, the type cache is stored under
        // LuaTypeOwner::Decl (keyed by position), not LuaTypeOwner::Member.
        // The member's syntax_id start position matches the decl's position, so
        // we can reconstruct the LuaDeclId and look up the type from there.
        for member_id in member_item.get_member_ids() {
            let decl_id = LuaDeclId::new(member_id.file_id, member_id.get_position());
            if let Some(type_cache) = db.get_type_index().get_type_cache(&decl_id.into()) {
                return Some(type_cache.as_type().clone());
            }
        }
    }

    None
}

fn infer_legacy_module_implicit_type(
    db: &DbIndex,
    file_id: FileId,
    position: TextSize,
    name: &str,
) -> Option<LuaType> {
    let module_env = db
        .get_module_index()
        .get_legacy_module_env_at(file_id, position)?;
    match name {
        "_M" => Some(LuaType::Namespace(
            smol_str::SmolStr::new(&module_env.module_path).into(),
        )),
        "_NAME" => Some(LuaType::StringConst(
            smol_str::SmolStr::new(&module_env.module_path).into(),
        )),
        "_PACKAGE" => Some(LuaType::StringConst(
            smol_str::SmolStr::new(module_env.package_name()).into(),
        )),
        _ => None,
    }
}

fn has_legacy_module_namespace(db: &DbIndex, name: &str) -> bool {
    db.get_module_index().has_legacy_module_namespace(name)
}

fn has_legacy_module_namespace_for_file(db: &DbIndex, file_id: Option<FileId>, name: &str) -> bool {
    file_id.is_some_and(|file_id| {
        db.get_module_index()
            .has_legacy_module_namespace_for_file(file_id, name)
    }) || file_id.is_none() && has_legacy_module_namespace(db, name)
}

fn select_realm_compatible_decl_ids_for_global_infer_tier(
    db: &DbIndex,
    call_state_mask: GmodStateMask,
    decl_ids: &[LuaDeclId],
) -> Vec<LuaDeclId> {
    let infer_index = db.get_gmod_infer_index();
    decl_ids
        .iter()
        .copied()
        .filter(|decl_id| {
            let decl_state_mask =
                infer_index.get_state_mask_at_offset(&decl_id.file_id, decl_id.position);
            call_state_mask.is_compatible_with(decl_state_mask)
        })
        .collect()
}

/// Resolves the full `self` reference identity for a `self` name expression.
///
/// Returns a [`SelfRefId`] carrying:
/// - `self_decl_id`: the (implicit or explicit) `self` declaration — unique per
///   method body, used as the flow-cache / `VarRefId` identity.
/// - `receiver`: the colon-method prefix owner used for base/member lookup.
///
/// For an explicit (shadowing) `self` local/param, the receiver is the `self`
/// decl itself, so it behaves like an ordinary local.
pub fn find_self_ref_id(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    name_expr: &LuaNameExpr,
) -> Option<SelfRefId> {
    let file_id = cache.get_file_id();
    let tree = db.get_decl_index().get_decl_tree(&file_id)?;

    let self_decl = tree.find_local_decl("self", name_expr.get_position())?;
    let self_decl_id = self_decl.get_id();
    if !self_decl.is_implicit_self() {
        return Some(SelfRefId {
            self_decl_id,
            receiver: LuaDeclOrMemberId::Decl(self_decl_id),
        });
    }

    let receiver = find_self_receiver_id(db, cache, &self_decl, name_expr)?;
    Some(SelfRefId {
        self_decl_id,
        receiver,
    })
}

/// Resolves the receiver owner (colon-method prefix) for an implicit `self`.
fn find_self_receiver_id(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    self_decl: &LuaDecl,
    name_expr: &LuaNameExpr,
) -> Option<LuaDeclOrMemberId> {
    let file_id = cache.get_file_id();
    let tree = db.get_decl_index().get_decl_tree(&file_id)?;

    let root = name_expr.get_root();
    let syntax_id = self_decl.get_syntax_id();
    let index_token = syntax_id.to_token_from_root(&root)?;
    let index_expr = LuaIndexExpr::cast(index_token.parent()?)?;
    let prefix_expr = index_expr.get_prefix_expr()?;

    match prefix_expr {
        LuaExpr::NameExpr(prefix_name) => {
            let name = prefix_name.get_name_text()?;
            let decl = tree.find_local_decl(&name, prefix_name.get_position());
            if let Some(decl) = decl {
                return Some(LuaDeclOrMemberId::Decl(decl.get_id()));
            }

            if name_expr_resolves_to_scoped_authoring_table(db, file_id, &prefix_name).is_some() {
                return Some(LuaDeclOrMemberId::Decl(self_decl.get_id()));
            }

            let id = resolve_global_decl_id(db, cache, &name, Some(&prefix_name))?;
            Some(LuaDeclOrMemberId::Decl(id))
        }
        LuaExpr::IndexExpr(prefix_index) => {
            let semantic_id = infer_node_semantic_decl(
                db,
                cache,
                prefix_index.syntax().clone(),
                SemanticDeclLevel::NoTrace,
            )?;

            match semantic_id {
                LuaSemanticDeclId::Member(member_id) => Some(LuaDeclOrMemberId::Member(member_id)),
                LuaSemanticDeclId::LuaDecl(decl_id) => Some(LuaDeclOrMemberId::Decl(decl_id)),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Returns true if the type contains an unresolved `SelfInfer`.
fn contains_self_infer(typ: &LuaType) -> bool {
    match typ {
        LuaType::SelfInfer => true,
        LuaType::TableOf(inner) => contains_self_infer(inner),
        LuaType::Union(u) => u.types().any(contains_self_infer),
        _ => false,
    }
}

/// Replaces `SelfInfer` with the given self type.
fn resolve_self_infer(typ: &LuaType, self_type: &LuaType) -> LuaType {
    match typ {
        LuaType::SelfInfer => self_type.clone(),
        LuaType::TableOf(inner) => LuaType::TableOf(Box::new(resolve_self_infer(inner, self_type))),
        LuaType::Union(u) => {
            let types: Vec<_> = u
                .types()
                .map(|t| resolve_self_infer(t, self_type))
                .collect();
            LuaType::Union(crate::LuaUnionType::from_vec(types).into())
        }
        _ => typ.clone(),
    }
}

/// Infers the self type from the enclosing method (colon function).
/// For `function ENT:Update() ... end`, this returns the type of `ENT`.
pub(crate) fn infer_enclosing_self_type(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    name_expr: &LuaNameExpr,
) -> Option<LuaType> {
    let func_stat = enclosing_colon_func_stat(name_expr)?;
    if let Some(authoritative_type) = infer_authoritative_method_self_type(db, cache, &func_stat) {
        return Some(authoritative_type);
    }

    infer_method_prefix_type(db, cache, &func_stat)
}

fn enclosing_colon_func_stat(name_expr: &LuaNameExpr) -> Option<LuaFuncStat> {
    name_expr.ancestors::<LuaFuncStat>().find(|func_stat| {
        matches!(
            func_stat.get_func_name(),
            Some(LuaVarExpr::IndexExpr(index_expr))
                if index_expr
                    .get_index_token()
                    .is_some_and(|token| token.is_colon())
        )
    })
}

fn infer_method_prefix_type(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    func_stat: &LuaFuncStat,
) -> Option<LuaType> {
    let LuaVarExpr::IndexExpr(index_expr) = func_stat.get_func_name()? else {
        return None;
    };
    if !index_expr
        .get_index_token()
        .is_some_and(|token| token.is_colon())
    {
        return None;
    }
    infer_expr(db, cache, index_expr.get_prefix_expr()?).ok()
}

#[cfg(test)]
mod test {
    use super::{
        direct_table_field_from_member_id, find_param_type_from_contextual_member,
        get_name_expr_var_ref_id, infer_name_expr,
    };
    use crate::{
        Emmyrc, LuaInferCache, LuaMemberId, LuaSignatureId, LuaType, LuaTypeCache, VarRefId,
        VirtualWorkspace,
    };
    use glua_parser::{
        LuaAstNode, LuaAstToken, LuaClosureExpr, LuaIndexKey, LuaLocalName, LuaNameExpr,
        LuaParamName, LuaTableExpr, LuaTableField,
    };
    use googletest::prelude::*;

    fn find_table_field(root: &glua_parser::LuaChunk, field_name: &str) -> LuaTableField {
        root.descendants::<LuaTableField>()
            .find(|field| {
                matches!(
                    field.get_field_key(),
                    Some(LuaIndexKey::Name(name)) if name.get_name_text() == field_name
                )
            })
            .expect("expected table field")
    }

    fn find_param_type_by_name(
        ws: &VirtualWorkspace,
        file_id: crate::FileId,
        name: &str,
    ) -> LuaType {
        find_param_types_by_name(ws, file_id, name)
            .into_iter()
            .next()
            .expect("expected semantic info for param")
    }

    fn find_param_types_by_name(
        ws: &VirtualWorkspace,
        file_id: crate::FileId,
        name: &str,
    ) -> Vec<LuaType> {
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("semantic model must exist");
        semantic_model
            .get_root()
            .descendants::<LuaParamName>()
            .filter(|param_name| {
                param_name
                    .get_name_token()
                    .is_some_and(|token| token.get_name_text() == name)
            })
            .filter_map(|param_name| {
                let token = param_name.get_name_token()?;
                semantic_model
                    .get_semantic_info(token.syntax().clone().into())
                    .map(|info| info.display_typ().clone())
            })
            .collect()
    }

    fn find_first_closure_signature_id(
        ws: &VirtualWorkspace,
        file_id: crate::FileId,
    ) -> LuaSignatureId {
        find_closure_signature_ids(ws, file_id)
            .into_iter()
            .next()
            .expect("expected closure")
    }

    fn find_closure_signature_ids(
        ws: &VirtualWorkspace,
        file_id: crate::FileId,
    ) -> Vec<LuaSignatureId> {
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("semantic model must exist");
        semantic_model
            .get_root()
            .descendants::<LuaClosureExpr>()
            .map(|closure| LuaSignatureId::from_closure(file_id, &closure))
            .collect()
    }

    #[gtest]
    fn test_infer_self_populates_name_var_ref_cache() -> Result<()> {
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def(
            r#"
            local PANEL = {}

            function PANEL:Init()
                local value = self
            end
            "#,
        );

        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("semantic model must exist");
        let self_expr = semantic_model
            .get_root()
            .descendants::<LuaNameExpr>()
            .find(|expr| expr.get_name_text().as_deref() == Some("self"))
            .expect("expected self name expr");
        let syntax_id = self_expr.get_syntax_id();

        let db = ws.analysis.compilation.get_db();
        let mut cache = LuaInferCache::new(file_id, Default::default());

        expect_that!(
            cache.expr_var_ref_id_cache.contains_key(&syntax_id),
            eq(false)
        );
        expect_that!(infer_name_expr(db, &mut cache, self_expr).is_ok(), eq(true));
        expect_that!(
            cache.expr_var_ref_id_cache.contains_key(&syntax_id),
            eq(true)
        );

        Ok(())
    }

    #[test]
    fn clear_for_unresolve_drops_global_var_ref_selected_from_mutable_types() {
        let mut ws = VirtualWorkspace::new();
        ws.def_file("a.lua", "Choice = 1");
        ws.def_file("b.lua", "Choice = function() end");
        let consumer_id = ws.def_file("consumer.lua", "local selected = Choice");

        let choice_expr = ws
            .analysis
            .compilation
            .get_semantic_model(consumer_id)
            .expect("consumer semantic model should exist")
            .get_root()
            .descendants::<LuaNameExpr>()
            .find(|expr| expr.get_name_text().as_deref() == Some("Choice"))
            .expect("consumer Choice expression should exist");
        let decl_ids = ws
            .analysis
            .compilation
            .get_db()
            .get_global_index()
            .get_global_decl_ids("Choice")
            .expect("Choice declarations should exist")
            .clone();
        assert_eq!(decl_ids.len(), 2);
        let first = decl_ids[0];
        let second = decl_ids[1];

        let db = ws.analysis.compilation.get_db_mut();
        db.get_type_index_mut()
            .force_bind_type(first.into(), LuaTypeCache::InferType(LuaType::Integer));
        db.get_type_index_mut()
            .force_bind_type(second.into(), LuaTypeCache::InferType(LuaType::Function));
        let mut cache = LuaInferCache::new(consumer_id, Default::default());
        assert_eq!(
            get_name_expr_var_ref_id(db, &mut cache, &choice_expr),
            Some(VarRefId::VarRef(second))
        );

        db.get_type_index_mut()
            .force_bind_type(first.into(), LuaTypeCache::InferType(LuaType::Function));
        cache.clear_for_unresolve(db);

        assert_eq!(
            get_name_expr_var_ref_id(db, &mut cache, &choice_expr),
            Some(VarRefId::VarRef(first))
        );
    }

    #[test]
    fn clear_for_unresolve_preserves_local_var_ref_identity() {
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def("local source = 1\nlocal selected = source");
        let source_reference = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("semantic model should exist")
            .get_root()
            .descendants::<LuaNameExpr>()
            .find(|expr| expr.get_name_text().as_deref() == Some("source"))
            .expect("source reference should exist");
        let syntax_id = source_reference.get_syntax_id();
        let db = ws.analysis.compilation.get_db();
        let mut cache = LuaInferCache::new(file_id, Default::default());

        let local_ref = get_name_expr_var_ref_id(db, &mut cache, &source_reference)
            .expect("source should resolve to a local reference");
        assert!(matches!(local_ref, VarRefId::VarRef(_)));

        cache.clear_for_unresolve(db);

        assert_eq!(
            cache.expr_var_ref_id_cache.get(&syntax_id),
            Some(&local_ref)
        );
    }

    #[gtest]
    fn test_contextual_member_param_type_inference_still_resolves_table_field_context() -> Result<()>
    {
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def(
            r#"
            ---@class HandlerSpec
            ---@field handle fun(value: string)

            ---@type HandlerSpec
            local handlers = {
                handle = function(value) end,
            }
            "#,
        );

        let inferred_type = find_param_type_by_name(&ws, file_id, "value");

        assert!(ws.check_type(&inferred_type, &LuaType::String));

        Ok(())
    }

    #[gtest]
    fn test_direct_table_field_lookup_returns_none_for_non_table_field_member_id() -> Result<()> {
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def(
            r#"
            local value = 1
            "#,
        );

        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("semantic model must exist");
        let root = semantic_model.get_root();
        let local_name = root
            .descendants::<LuaLocalName>()
            .find(|name| {
                name.get_name_token()
                    .is_some_and(|token| token.get_name_text() == "value")
            })
            .expect("expected local name");
        let member_id = LuaMemberId::new(local_name.get_syntax_id(), file_id);

        let direct = direct_table_field_from_member_id(&root, member_id);
        let scan = root
            .descendants::<LuaTableField>()
            .find(|field| field.get_syntax_id() == *member_id.get_syntax_id());

        assert!(direct.is_none());
        assert!(scan.is_none());

        Ok(())
    }

    #[gtest]
    fn test_contextual_member_self_param_uses_same_vgui_parent_table() -> Result<()> {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def_gmod_call_arg_builtins();

        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        let file_id = ws.def(
            r#"
            local PANEL = {
                Init = function(self) end,
            }

            vgui.Register("MyPanel", PANEL, "Panel")
            "#,
        );

        let db = ws.analysis.compilation.get_db();
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("semantic model must exist");
        let root = semantic_model.get_root();
        let table_field = find_table_field(&root, "Init");
        let member_id = LuaMemberId::new(table_field.get_syntax_id(), file_id);
        let parent_table = table_field
            .get_parent::<LuaTableExpr>()
            .expect("expected parent table");

        let mut parent_cache = LuaInferCache::new(file_id, Default::default());
        let parent_type = super::infer_table_should_be(db, &mut parent_cache, parent_table)
            .expect("expected inferred parent table type");

        let mut contextual_cache = LuaInferCache::new(file_id, Default::default());
        let contextual_self_type = find_param_type_from_contextual_member(
            db,
            &mut contextual_cache,
            member_id,
            0,
            false,
            true,
            false,
        )
        .expect("expected inferred contextual self type");

        assert_eq!(contextual_self_type, parent_type);

        Ok(())
    }

    #[gtest]
    fn test_local_function_call_site_param_inference_still_resolves() -> Result<()> {
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def(
            r#"
            local function takes_value(value)
                return value
            end

            takes_value("hello")
            "#,
        );

        let inferred_type = find_param_type_by_name(&ws, file_id, "value");

        assert!(ws.check_type(&inferred_type, &LuaType::String));

        Ok(())
    }

    #[gtest]
    fn test_call_site_param_inference_uses_same_file_signature_when_names_collide() -> Result<()> {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc
            .gmod
            .file_param_defaults
            .insert("veh".to_string(), "DefaultVehicle".to_string());
        ws.update_emmyrc(emmyrc);

        let file_ids = ws.def_files(vec![
            (
                "file_a.lua",
                r#"
                function Handle(veh)
                    local snapshot_a = veh
                    return snapshot_a
                end

                Handle("sedan")
                "#,
            ),
            (
                "file_b.lua",
                r#"
                function Handle(veh)
                    local snapshot_b = veh
                    return snapshot_b
                end

                Handle(123)
                "#,
            ),
        ]);

        let file_a_signature = find_first_closure_signature_id(&ws, file_ids[0]);
        let file_b_signature = find_first_closure_signature_id(&ws, file_ids[1]);
        let (file_a_param, file_b_param) = {
            let db = ws.analysis.compilation.get_db();
            (
                db.get_call_site_param_index()
                    .get_inferred_param(&file_a_signature, 0)
                    .cloned()
                    .expect("expected call-site evidence for file_a"),
                db.get_call_site_param_index()
                    .get_inferred_param(&file_b_signature, 0)
                    .cloned()
                    .expect("expected call-site evidence for file_b"),
            )
        };

        assert!(ws.check_type(&file_a_param, &LuaType::String));
        assert!(
            matches!(file_b_param, LuaType::Integer | LuaType::IntegerConst(_)),
            "expected integer evidence for file_b, got {file_b_param:?}"
        );

        Ok(())
    }

    #[gtest]
    fn test_call_site_param_inference_uses_visible_signature_before_redefinition() -> Result<()> {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc
            .gmod
            .file_param_defaults
            .insert("veh".to_string(), "DefaultVehicle".to_string());
        ws.update_emmyrc(emmyrc);

        let file_id = ws.def(
            r#"
            function Handle(veh)
                local snapshot_a = veh
                return snapshot_a
            end

            Handle("sedan")

            function Handle(veh)
                local snapshot_b = veh
                return snapshot_b
            end

            Handle(123)
            "#,
        );

        let signatures = find_closure_signature_ids(&ws, file_id);
        assert_that!(signatures.len(), eq(2));

        let (first_param, second_param) = {
            let db = ws.analysis.compilation.get_db();
            (
                db.get_call_site_param_index()
                    .get_inferred_param(&signatures[0], 0)
                    .cloned()
                    .expect("expected call-site evidence for first Handle"),
                db.get_call_site_param_index()
                    .get_inferred_param(&signatures[1], 0)
                    .cloned()
                    .expect("expected call-site evidence for second Handle"),
            )
        };

        assert!(ws.check_type(&first_param, &LuaType::String));
        assert!(ws.check_type(&second_param, &LuaType::Integer));

        Ok(())
    }

    #[gtest]
    fn test_non_local_closure_signature_does_not_bind_local_function_decl() -> Result<()> {
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def(
            r#"
            local callback = function(value)
                return value
            end
            "#,
        );

        let signature_id = find_first_closure_signature_id(&ws, file_id);
        let db = ws.analysis.compilation.get_db();

        assert_eq!(
            db.get_signature_index().local_func_decl_for(&signature_id),
            None
        );

        Ok(())
    }

    #[gtest]
    fn test_local_function_signature_mapping_updates_after_reparse() -> Result<()> {
        let mut ws = VirtualWorkspace::new();
        let uri = ws.virtual_url_generator.new_uri("reparse.lua");
        let file_id = ws
            .analysis
            .update_file_by_uri(
                &uri,
                Some(
                    r#"
                    local function first(value)
                        return value
                    end
                    "#
                    .to_string(),
                ),
            )
            .expect("file id must be present");

        let initial_signature_id = find_first_closure_signature_id(&ws, file_id);
        let initial_decl_id = ws
            .analysis
            .compilation
            .get_db()
            .get_signature_index()
            .local_func_decl_for(&initial_signature_id)
            .expect("expected initial local function decl mapping");

        ws.analysis.update_file_by_uri(
            &uri,
            Some(
                r#"
                -- shift signature position to verify stale map cleanup
                local function replacement(value)
                    return value
                end
                "#
                .to_string(),
            ),
        );

        let updated_signature_id = find_first_closure_signature_id(&ws, file_id);
        let db = ws.analysis.compilation.get_db();
        let updated_decl_id = db
            .get_signature_index()
            .local_func_decl_for(&updated_signature_id)
            .expect("expected updated local function decl mapping");

        assert_eq!(
            db.get_signature_index()
                .local_func_decl_for(&initial_signature_id),
            None
        );
        assert_ne!(initial_decl_id, updated_decl_id);

        Ok(())
    }

    #[gtest]
    fn test_adjacent_local_functions_keep_distinct_decl_bindings() -> Result<()> {
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def(
            r#"
            local function first(value)
                return value
            end

            local function second(value)
                return value
            end

            first("text")
            second(123)
            "#,
        );

        let db = ws.analysis.compilation.get_db();
        let root = db
            .get_vfs()
            .get_syntax_tree(&file_id)
            .expect("expected syntax tree")
            .get_red_root();
        let signature_ids = root
            .descendants()
            .filter_map(LuaClosureExpr::cast)
            .map(|closure| LuaSignatureId::from_closure(file_id, &closure))
            .collect::<Vec<_>>();

        assert_eq!(signature_ids.len(), 2);
        let first_decl_id = db
            .get_signature_index()
            .local_func_decl_for(&signature_ids[0])
            .expect("expected first local func decl");
        let second_decl_id = db
            .get_signature_index()
            .local_func_decl_for(&signature_ids[1])
            .expect("expected second local func decl");

        assert_ne!(first_decl_id, second_decl_id);
        let param_types = find_param_types_by_name(&ws, file_id, "value");
        assert_eq!(param_types.len(), 2);
        assert!(ws.check_type(&param_types[0], &LuaType::String));
        assert!(ws.check_type(&param_types[1], &LuaType::Number));

        Ok(())
    }
}
