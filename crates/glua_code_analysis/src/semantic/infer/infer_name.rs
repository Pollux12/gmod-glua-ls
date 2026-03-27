use std::path::Path;

use glua_parser::{LuaAstNode, LuaExpr, LuaFuncStat, LuaIndexExpr, LuaNameExpr, LuaVarExpr};
use rowan::TextSize;
use wax::Pattern;

use super::{InferFailReason, InferResult, infer_expr};
use crate::{
    FileId, GmodRealm, LuaDecl, LuaDeclExtra, LuaDeclId, LuaInferCache, LuaMemberId,
    LuaSemanticDeclId, LuaType, LuaTypeDeclId, SemanticDeclLevel, TypeOps,
    db_index::{DbIndex, LuaDeclOrMemberId},
    infer_node_semantic_decl,
    semantic::{
        infer::narrow::{VarRefId, infer_expr_narrow_type},
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
        "self" => return infer_self(db, cache, name_expr),
        "_G" => return Ok(LuaType::Global),
        _ => {}
    }

    let file_id = cache.get_file_id();
    let references_index = db.get_reference_index();
    let range = name_expr.get_range();
    let decl_id = references_index
        .get_local_reference(&file_id)
        .and_then(|file_ref| file_ref.get_decl_id(&range));
    let result = if let Some(decl_id) = decl_id {
        infer_expr_narrow_type(
            db,
            cache,
            LuaExpr::NameExpr(name_expr.clone()),
            VarRefId::VarRef(decl_id),
        )
    } else {
        if let Some(define_baseclass_type) = infer_define_baseclass_type(db, file_id, name) {
            return Ok(define_baseclass_type);
        }

        if let Some(scoped_type) = infer_scoped_scripted_global_type(db, cache, name) {
            return Ok(scoped_type);
        }

        match get_name_expr_var_ref_id(db, cache, &name_expr) {
            Some(VarRefId::GlobalName(_, _)) => infer_expr_narrow_type(
                db,
                cache,
                LuaExpr::NameExpr(name_expr.clone()),
                VarRefId::GlobalName(
                    internment::ArcIntern::new(smol_str::SmolStr::new(name)),
                    name_expr.get_position(),
                ),
            )
            .or_else(|_| {
                infer_global_type(db, Some(file_id), Some(name_expr.get_position()), name)
            }),
            Some(_) | None => {
                infer_global_type(db, Some(file_id), Some(name_expr.get_position()), name)
            }
        }
    };

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

fn infer_define_baseclass_type(db: &DbIndex, file_id: FileId, name: &str) -> Option<LuaType> {
    if !db.get_emmyrc().gmod.enabled || name != "BaseClass" {
        return None;
    }

    let base_name = db
        .get_gmod_class_metadata_index()
        .get_define_baseclass_name(&file_id)?;
    Some(LuaType::Ref(LuaTypeDeclId::global(base_name)))
}

fn infer_scoped_scripted_global_type(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    name: &str,
) -> Option<LuaType> {
    let class_decl_id = resolve_scoped_scripted_global_type_decl_id(db, cache, name)?;
    Some(LuaType::Def(class_decl_id))
}

pub(crate) fn resolve_scoped_scripted_global_type_decl_id(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    name: &str,
) -> Option<LuaTypeDeclId> {
    if !db.get_emmyrc().gmod.enabled
        || !db
            .get_emmyrc()
            .gmod
            .scripted_class_scopes
            .resolved_definitions()
            .iter()
            .any(|definition| definition.class_global == name)
    {
        return None;
    }

    let (global_name, class_name) = detect_scoped_global_from_path_cached(db, cache)?;
    if global_name != name {
        return None;
    }

    Some(LuaTypeDeclId::global(&class_name))
}

fn detect_scoped_global_from_path_cached(
    db: &DbIndex,
    cache: &mut LuaInferCache,
) -> Option<(String, String)> {
    if let Some(cached) = cache.scoped_scripted_global_cache.as_ref() {
        return cached.clone();
    }

    let detected = detect_scoped_global_from_path(db, cache.get_file_id());
    cache.scoped_scripted_global_cache = Some(detected.clone());
    detected
}

fn detect_scoped_global_from_path(db: &DbIndex, file_id: FileId) -> Option<(String, String)> {
    if !is_in_scripted_class_scope(db, file_id) {
        return None;
    }

    let file_path = db.get_vfs().get_file_path(&file_id)?;
    let scope_match = db
        .get_emmyrc()
        .gmod
        .scripted_class_scopes
        .detect_class_for_path(file_path)?;

    Some((scope_match.definition.class_global, scope_match.class_name))
}

fn is_in_scripted_class_scope(db: &DbIndex, file_id: FileId) -> bool {
    let scopes = &db.get_emmyrc().gmod.scripted_class_scopes;
    let include_patterns = scopes.include_patterns();
    let exclude_patterns = scopes.exclude_patterns();
    if include_patterns.is_empty() && exclude_patterns.is_empty() {
        return true;
    }

    let Some(file_path) = db.get_vfs().get_file_path(&file_id) else {
        return include_patterns.is_empty();
    };

    let normalized_path = file_path.to_string_lossy().replace('\\', "/");
    let mut candidate_paths = Vec::new();
    push_path_candidates(&mut candidate_paths, &normalized_path);
    let normalized_lower = normalized_path.to_ascii_lowercase();
    if let Some(lua_idx) = normalized_lower.find("/lua/") {
        let lua_relative_path = normalized_path[lua_idx + 1..].to_string();
        push_path_candidates(&mut candidate_paths, &lua_relative_path);
        if let Some(stripped) = lua_relative_path.strip_prefix("lua/") {
            push_path_candidates(&mut candidate_paths, stripped);
        }
    }
    if let Some(file_name) = file_path.file_name().and_then(|name| name.to_str()) {
        push_candidate_path(&mut candidate_paths, file_name);
    }

    if !include_patterns.is_empty() {
        let include_set = match wax::any(
            include_patterns
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
        ) {
            Ok(glob) => glob,
            Err(err) => {
                log::warn!("Invalid gmod.scriptedClassScopes.include pattern: {err}");
                return true;
            }
        };
        if !candidate_paths
            .iter()
            .any(|path| include_set.is_match(Path::new(path)))
        {
            return false;
        }
    }

    if !exclude_patterns.is_empty() {
        let exclude_set = match wax::any(
            exclude_patterns
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
        ) {
            Ok(glob) => glob,
            Err(err) => {
                log::warn!("Invalid gmod.scriptedClassScopes.exclude pattern: {err}");
                return false;
            }
        };
        if candidate_paths
            .iter()
            .any(|path| exclude_set.is_match(Path::new(path)))
        {
            return false;
        }
    }

    true
}

fn push_path_candidates(candidate_paths: &mut Vec<String>, path: &str) {
    push_candidate_path(candidate_paths, path);

    let segments = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    for idx in 0..segments.len() {
        push_candidate_path(candidate_paths, &segments[idx..].join("/"));
    }
}

fn push_candidate_path(candidate_paths: &mut Vec<String>, candidate: &str) {
    if candidate.is_empty() {
        return;
    }

    if candidate_paths.iter().any(|existing| existing == candidate) {
        return;
    }

    candidate_paths.push(candidate.to_string());
}

fn infer_self(db: &DbIndex, cache: &mut LuaInferCache, name_expr: LuaNameExpr) -> InferResult {
    if let Some(scoped_self_type) = infer_scoped_implicit_self_type(db, cache, &name_expr) {
        return Ok(scoped_self_type);
    }

    let decl_or_member_id =
        find_self_decl_or_member_id(db, cache, &name_expr).ok_or(InferFailReason::None)?;
    // LuaDeclOrMemberId::Member(member_id) => find_decl_member_type(db, member_id),
    infer_expr_narrow_type(
        db,
        cache,
        LuaExpr::NameExpr(name_expr),
        VarRefId::SelfRef(decl_or_member_id),
    )
}

fn infer_scoped_implicit_self_type(
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

    // Check self type cache for this method
    if let Some(cached) = cache.self_type_cache.get(&func_syntax_id) {
        return cached.clone();
    }

    let result = infer_scoped_implicit_self_type_inner(db, cache, func_stat);

    // Cache the result for subsequent `self` references in the same method
    cache.self_type_cache.insert(func_syntax_id, result.clone());
    result
}

fn infer_scoped_implicit_self_type_inner(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    func_stat: LuaFuncStat,
) -> Option<LuaType> {
    let func_name = func_stat.get_func_name()?;
    let LuaVarExpr::IndexExpr(index_expr) = func_name else {
        return None;
    };
    if !index_expr.get_index_token()?.is_colon() {
        return None;
    }

    let LuaExpr::NameExpr(prefix_name_expr) = index_expr.get_prefix_expr()? else {
        return None;
    };
    let prefix_name = prefix_name_expr.get_name_text()?;
    let class_decl_id = resolve_scoped_scripted_global_type_decl_id(db, cache, &prefix_name)?;
    Some(LuaType::Def(class_decl_id))
}

pub fn get_name_expr_var_ref_id(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    name_expr: &LuaNameExpr,
) -> Option<VarRefId> {
    let name_token = name_expr.get_name_token()?;
    let name = name_token.get_name_text();
    match name {
        "self" => {
            let decl_or_id = find_self_decl_or_member_id(db, cache, name_expr)?;
            Some(VarRefId::SelfRef(decl_or_id))
        }
        _ => {
            let file_id = cache.get_file_id();
            let references_index = db.get_reference_index();
            let range = name_expr.get_range();
            if let Some(decl_id) = references_index
                .get_local_reference(&file_id)
                .and_then(|file_ref| file_ref.get_decl_id(&range))
            {
                return Some(VarRefId::VarRef(decl_id));
            }

            if let Some(global_decl_id) = resolve_global_decl_id(db, cache, name, Some(name_expr)) {
                return Some(VarRefId::VarRef(global_decl_id));
            }

            Some(VarRefId::GlobalName(
                internment::ArcIntern::new(smol_str::SmolStr::new(name)),
                name_expr.get_position(),
            ))
        }
    }
}

pub fn infer_param(db: &DbIndex, decl: &LuaDecl) -> InferResult {
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

            return Ok(typ);
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
            return Ok(param_type);
        }
    }

    if let Some(file_hint_type) = infer_param_type_from_file_hint(db, decl) {
        return Ok(file_hint_type);
    }

    if let Some(param_hint_type) = infer_param_type_from_gmod_name_hint(db, decl.get_name()) {
        return Ok(param_hint_type);
    }

    Err(InferFailReason::UnResolveDeclType(decl.get_id()))
}

fn infer_param_type_from_gmod_name_hint(db: &DbIndex, param_name: &str) -> Option<LuaType> {
    if !db.get_emmyrc().gmod.enabled {
        return None;
    }

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
    if !db.get_emmyrc().gmod.enabled {
        return None;
    }

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
                    Some(typ)
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
                    Some(typ)
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

pub fn infer_global_type(
    db: &DbIndex,
    current_file_id: Option<FileId>,
    call_offset: Option<TextSize>,
    name: &str,
) -> InferResult {
    let module_index = db.get_module_index();
    let global_index = db.get_global_index();
    let priority_tiers = if let Some(current_workspace_id) =
        current_file_id.and_then(|file_id| module_index.get_workspace_id(file_id))
    {
        global_index
            .get_global_decl_id_priority_tiers(name, module_index, current_workspace_id)
            .ok_or(InferFailReason::None)?
    } else {
        vec![(
            0,
            global_index
                .get_global_decl_ids(name)
                .cloned()
                .ok_or(InferFailReason::None)?,
        )]
    };

    let decl_ids =
        select_decl_ids_for_global_infer(db, current_file_id, call_offset, &priority_tiers);
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

    // TODO: 或许应该联合所有定义的类型?
    let mut valid_type = LuaType::Unknown;
    let mut last_resolve_reason = InferFailReason::None;
    for decl_id in sorted_decl_ids {
        let decl_type_cache = db.get_type_index().get_type_cache(&decl_id.into());
        match decl_type_cache {
            Some(type_cache) => {
                let typ = type_cache.as_type();

                if typ.contain_tpl() {
                    // This decl is located in a generic function,
                    // and is type contains references to generic variables
                    // of this function.
                    continue;
                }

                if typ.is_def() || typ.is_ref() {
                    return Ok(typ.clone());
                }

                if typ.is_function() {
                    valid_type = TypeOps::Union.apply(db, &valid_type, typ);
                }

                if type_cache.is_table() {
                    valid_type = typ.clone();
                }
            }
            None => {
                last_resolve_reason = InferFailReason::UnResolveDeclType(decl_id);
            }
        }
    }

    if !valid_type.is_unknown() {
        return Ok(valid_type);
    }

    Err(last_resolve_reason)
}

fn select_decl_ids_for_global_infer(
    db: &DbIndex,
    current_file_id: Option<FileId>,
    call_offset: Option<TextSize>,
    priority_tiers: &[(u8, Vec<LuaDeclId>)],
) -> Vec<LuaDeclId> {
    let Some((_, best_tier_decl_ids)) = priority_tiers.first() else {
        return Vec::new();
    };

    if !db.get_emmyrc().gmod.enabled {
        return best_tier_decl_ids.clone();
    }

    let (Some(file_id), Some(call_offset)) = (current_file_id, call_offset) else {
        return best_tier_decl_ids.clone();
    };

    let infer_index = db.get_gmod_infer_index();
    let call_realm = infer_index.get_realm_at_offset(&file_id, call_offset);
    for (_, decl_ids) in priority_tiers {
        let mut compatible_decl_ids = Vec::new();
        for decl_id in decl_ids {
            let decl_realm = infer_index.get_realm_at_offset(&decl_id.file_id, decl_id.position);
            if is_realm_compatible(call_realm, decl_realm) {
                compatible_decl_ids.push(*decl_id);
            }
        }

        if !compatible_decl_ids.is_empty() {
            return compatible_decl_ids;
        }
    }

    best_tier_decl_ids.clone()
}

fn is_realm_compatible(call_realm: GmodRealm, decl_realm: GmodRealm) -> bool {
    !matches!(
        (call_realm, decl_realm),
        (GmodRealm::Client, GmodRealm::Server) | (GmodRealm::Server, GmodRealm::Client)
    )
}

pub fn find_self_decl_or_member_id(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    name_expr: &LuaNameExpr,
) -> Option<LuaDeclOrMemberId> {
    let file_id = cache.get_file_id();
    let tree = db.get_decl_index().get_decl_tree(&file_id)?;

    let self_decl = tree.find_local_decl("self", name_expr.get_position())?;
    if !self_decl.is_implicit_self() {
        return Some(LuaDeclOrMemberId::Decl(self_decl.get_id()));
    }

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
        LuaType::Union(u) => u.into_vec().iter().any(contains_self_infer),
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
                .into_vec()
                .iter()
                .map(|t| resolve_self_infer(t, self_type))
                .collect();
            LuaType::Union(crate::LuaUnionType::from_vec(types).into())
        }
        _ => typ.clone(),
    }
}

/// Infers the self type from the enclosing method (colon function).
/// For `function ENT:Update() ... end`, this returns the type of `ENT`.
fn infer_enclosing_self_type(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    name_expr: &LuaNameExpr,
) -> Option<LuaType> {
    for func_stat in name_expr.ancestors::<LuaFuncStat>() {
        let func_name = func_stat.get_func_name()?;
        if let LuaVarExpr::IndexExpr(index_expr) = func_name {
            if index_expr.get_index_token()?.is_colon() {
                let prefix_expr = index_expr.get_prefix_expr()?;
                return infer_expr(db, cache, prefix_expr).ok();
            }
        }
    }
    None
}
