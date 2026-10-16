use super::StatementWrite;
use glua_parser::{
    LuaAssignStat, LuaAstNode, LuaAstToken, LuaExpr, LuaForRangeStat, LuaIndexExpr, LuaIndexKey,
    LuaNameExpr, PathTrait,
};
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};

use crate::{
    DbIndex, FileId, FuncGenericBinding, FuncGenericBindingState, GenericTplId, InferFailReason,
    LuaAliasCallKind, LuaAliasCallType, LuaDeclId, LuaInferCache, LuaMemberId, LuaMemberInfo,
    LuaMemberKey, LuaMemberOwner, LuaObjectType, LuaOperatorMetaMethod, LuaSemanticDeclId, LuaType,
    LuaTypeCache, TplContext, TypeOps, TypeSubstitutor, VariadicType,
    compilation::analyzer::{
        common::TypeCacheWriteMode, dynamic_field::for_range_pairs_source_for_var,
        is_provably_builtin_iterator_call, writes_own_pairs_key,
    },
    get_member_map, infer_expr, instantiate_doc_function,
    semantic::get_buildin_type_map_type_id,
    tpl_pattern_match_args,
};

use super::LuaAnalyzer;

pub fn analyze_for_range_stat(
    analyzer: &mut LuaAnalyzer,
    for_range_stat: LuaForRangeStat,
) -> Option<()> {
    let var_name_list = for_range_stat.get_var_name_list().collect::<Vec<_>>();
    let iter_exprs = for_range_stat.get_expr_list().collect::<Vec<_>>();
    let cache = analyzer
        .context
        .infer_manager
        .get_infer_cache(analyzer.file_id);
    let iter_var_types = infer_for_range_iter_expr_func(&analyzer.db, cache, &iter_exprs);
    let enumerates_member_map =
        iterates_table_member_map(&analyzer.db, analyzer.file_id, &for_range_stat);

    match iter_var_types {
        Ok(iter_var_types) => {
            for (idx, var_name) in var_name_list.iter().enumerate() {
                let position = var_name.get_position();
                let decl_id = LuaDeclId::new(analyzer.file_id, position);
                let ret_type = iter_var_types
                    .get_type(idx)
                    .cloned()
                    .unwrap_or(LuaType::Unknown);
                let ret_type = TypeOps::Remove.apply(&analyzer.db, &ret_type, &LuaType::Nil);
                analyzer.write(StatementWrite::WriteTypeCache {
                    owner: decl_id.into(),
                    cache: LuaTypeCache::InferType(ret_type),
                    mode: TypeCacheWriteMode::InsertOnly,
                });
            }

            if iter_var_types.degraded_unbound_tpl || enumerates_member_map {
                // Either a generic iterator left templates nothing bound, so
                // the vars hold degraded placeholders, or the types came from
                // enumerating a table's member map. The bound arms, or the
                // enumerated union, replace what the insert kept when they
                // widen it.
                let var_positions = var_name_list
                    .iter()
                    .map(LuaAstToken::get_position)
                    .collect::<Vec<_>>();
                analyzer.write(StatementWrite::IterVarUpdates {
                    file_id: analyzer.file_id,
                    iter_var_types: iter_var_types.clone(),
                    var_positions,
                });
            }
        }
        Err(InferFailReason::None) => {
            for var_name in var_name_list {
                let position = var_name.get_position();
                let decl_id = LuaDeclId::new(analyzer.file_id, position);
                analyzer.write(StatementWrite::WriteTypeCache {
                    owner: decl_id.into(),
                    cache: LuaTypeCache::InferType(LuaType::Unknown),
                    mode: TypeCacheWriteMode::InsertOnly,
                });
            }
        }
        // Nothing to write: the statement runs again once a fact the iterator
        // read changes.
        Err(_) => {}
    }

    Some(())
}

/// Whether this loop's variable types come from enumerating a table's
/// member map, the union [`try_infer_pairs_iter_types_from_table_members`]
/// builds.
pub fn iterates_table_member_map(
    db: &DbIndex,
    file_id: FileId,
    for_range_stat: &LuaForRangeStat,
) -> bool {
    let Some(LuaExpr::CallExpr(call_expr)) = for_range_stat.get_expr_list().next() else {
        return false;
    };
    is_global_pairs_call(db, file_id, &call_expr)
}

#[derive(Debug, Clone)]
pub struct IterVarInference {
    pub types: VariadicType,
    pub settled_overrides: Vec<Option<LuaType>>,
    /// Whether some slot carried an unbound template, degraded to its
    /// constraint or [`LuaType::Unknown`]. Such an answer was taken against
    /// the index as it stood when the iterator was inferred, so the loop vars
    /// stay queued for settled re-derivation.
    pub degraded_unbound_tpl: bool,
}

impl IterVarInference {
    pub fn get_type(&self, idx: usize) -> Option<&LuaType> {
        self.types.get_type(idx)
    }
}

pub fn infer_for_range_iter_expr_func(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    iter_exprs: &[LuaExpr],
) -> Result<IterVarInference, InferFailReason> {
    if iter_exprs.is_empty() {
        return Err(InferFailReason::None);
    }

    let mut status_param = None;
    if iter_exprs.len() > 1 {
        let status_param_expr = iter_exprs[1].clone();
        status_param = Some(infer_expr(db, cache, status_param_expr)?);
    }

    let iter_func_expr = iter_exprs[0].clone();
    let first_expr_type = infer_expr(db, cache, iter_func_expr)?;
    if let Some(iter_types) =
        try_infer_pairs_iter_types_from_table_members(db, cache, &iter_exprs[0], &first_expr_type)?
    {
        return Ok(IterVarInference {
            types: iter_types,
            settled_overrides: Vec::new(),
            degraded_unbound_tpl: false,
        });
    }

    let builtin_iter_bindings = if let Some(LuaExpr::CallExpr(call_expr)) = iter_exprs.first()
        && is_provably_builtin_iterator_call(db, cache.get_file_id(), call_expr)
    {
        cache
            .get_generic_call_bindings(&call_expr.get_syntax_id())
            .map(|bindings| bindings.to_vec())
    } else {
        None
    };

    let doc_function = match first_expr_type {
        LuaType::DocFunction(func) => func,
        LuaType::Signature(sig_id) => {
            let sig = db
                .get_signature_index()
                .get(&sig_id)
                .ok_or(InferFailReason::None)?;
            if !sig.is_resolve_return() {
                return Err(InferFailReason::UnResolveSignatureReturn(sig_id));
            }
            sig.to_doc_func_type()
        }
        LuaType::Ref(type_decl_id) => {
            let type_decl = db
                .get_type_index()
                .get_type_decl(&type_decl_id)
                .ok_or(InferFailReason::None)?;
            if type_decl.is_alias() {
                let alias_origin = type_decl
                    .get_alias_origin(db, None)
                    .ok_or(InferFailReason::None)?;
                match alias_origin {
                    LuaType::DocFunction(doc_func) => doc_func,
                    _ => return Err(InferFailReason::None),
                }
            } else if type_decl.is_class() {
                let operator_index = db.get_operator_index();
                let operator_ids = operator_index
                    .get_operators(&type_decl_id.into(), LuaOperatorMetaMethod::Call)
                    .ok_or(InferFailReason::None)?;
                operator_ids
                    .iter()
                    .filter_map(|overload_id| {
                        let operator = operator_index.get_operator(overload_id)?;
                        let func = operator.get_operator_func(db);
                        match func {
                            LuaType::DocFunction(f) => Some(f.clone()),
                            _ => None,
                        }
                    })
                    .next()
                    .ok_or(InferFailReason::None)?
            } else {
                return Err(InferFailReason::None);
            }
        }
        LuaType::Variadic(multi) => {
            let first_type = multi.get_type(0).cloned().unwrap_or(LuaType::Unknown);
            let second_type = multi.get_type(1).cloned().unwrap_or(LuaType::Unknown);
            if !second_type.is_unknown() {
                status_param = Some(second_type);
            }

            match first_type {
                LuaType::DocFunction(func) => func,
                _ => return Err(InferFailReason::None),
            }
        }
        _ => return Err(InferFailReason::None),
    };

    let Some(status_param) = status_param else {
        let types = doc_function.get_variadic_ret();
        let settled_overrides =
            compute_proven_settled_overrides(builtin_iter_bindings.as_deref(), &types);
        let (types, degraded_unbound_tpl) =
            substitute_unbound_iter_var_templates(types, &settled_overrides);
        return Ok(IterVarInference {
            types,
            settled_overrides,
            degraded_unbound_tpl,
        });
    };
    let mut substitutor = TypeSubstitutor::new();
    let mut context = TplContext {
        db,
        cache,
        substitutor: &mut substitutor,
        call_expr: None,
        source_range: iter_exprs[0].get_range(),
    };
    let params = doc_function
        .get_params()
        .iter()
        .map(|(_, opt_ty)| opt_ty.clone().unwrap_or(LuaType::Any))
        .collect::<Vec<_>>();

    tpl_pattern_match_args(&mut context, &params, &[status_param])?;

    let instantiate_func = if let LuaType::DocFunction(f) =
        instantiate_doc_function(db, &doc_function, &substitutor)
    {
        f
    } else {
        doc_function
    };

    let types = instantiate_func.get_variadic_ret();
    let settled_overrides =
        compute_proven_settled_overrides(builtin_iter_bindings.as_deref(), &types);
    let (types, degraded_unbound_tpl) =
        substitute_unbound_iter_var_templates(types, &settled_overrides);
    Ok(IterVarInference {
        types,
        settled_overrides,
        degraded_unbound_tpl,
    })
}

/// Substitutes the settled overrides into the iterator's return slots and
/// degrades every remaining unbound template. A raw `TplRef` is a placeholder
/// an unbound generic left behind, never a type the variable holds, so it must
/// never reach the decl cache: it degrades to its declared constraint, or to
/// `Unknown` when none exists. Returns whether any slot carried an unbound
/// template — such an answer was taken against the index as it stood when the
/// iterator was inferred, so it must be re-derived once the rest has landed.
fn substitute_unbound_iter_var_templates(
    types: VariadicType,
    settled_overrides: &[Option<LuaType>],
) -> (VariadicType, bool) {
    match types {
        VariadicType::Multi(list) => {
            let mut degraded_unbound_tpl = false;
            let substituted = list
                .into_iter()
                .enumerate()
                .map(|(idx, typ)| {
                    if !typ.contain_tpl() {
                        return typ;
                    }
                    degraded_unbound_tpl = true;
                    let result = match settled_overrides.get(idx) {
                        Some(Some(fallback)) if !fallback.contain_tpl() => fallback.clone(),
                        _ => degrade_unbound_iter_var_type(typ),
                    };
                    if result.contain_tpl() {
                        LuaType::Unknown
                    } else {
                        result
                    }
                })
                .collect();
            (VariadicType::Multi(substituted), degraded_unbound_tpl)
        }
        VariadicType::Base(typ) => {
            if !typ.contain_tpl() {
                return (VariadicType::Base(typ), false);
            }
            if let Some(Some(fallback)) = settled_overrides.first() {
                if !fallback.contain_tpl() {
                    return (VariadicType::Base(fallback.clone()), true);
                }
            }
            let degraded = degrade_unbound_iter_var_type(typ);
            let final_type = if degraded.contain_tpl() {
                LuaType::Unknown
            } else {
                degraded
            };
            (VariadicType::Base(final_type), true)
        }
    }
}

/// Degrade an unbound template placeholder to a real type: its declared
/// constraint when one exists and is concrete, else `Unknown`. Union arms that
/// did bind survive; only the placeholder arms are dropped.
fn degrade_unbound_iter_var_type(typ: LuaType) -> LuaType {
    let degraded = match typ {
        LuaType::TplRef(tpl) | LuaType::ConstTplRef(tpl) => match tpl.get_constraint() {
            Some(constraint) if !constraint.contain_tpl() => constraint.clone(),
            _ => LuaType::Unknown,
        },
        LuaType::StrTplRef(tpl) => match tpl.get_constraint() {
            Some(constraint) if !constraint.contain_tpl() => constraint.clone(),
            _ => LuaType::Unknown,
        },
        LuaType::Union(union) => {
            let bound = union
                .types()
                .filter(|arm| !arm.contain_tpl())
                .cloned()
                .collect::<Vec<_>>();
            match bound.as_slice() {
                [] => LuaType::Unknown,
                [only] => only.clone(),
                _ => LuaType::from_vec(bound),
            }
        }
        typ => {
            if typ.contain_tpl() {
                LuaType::Unknown
            } else {
                typ
            }
        }
    };
    if degraded.contain_tpl() {
        LuaType::Unknown
    } else {
        degraded
    }
}

fn compute_proven_settled_overrides(
    bindings: Option<&[FuncGenericBinding]>,
    types: &VariadicType,
) -> Vec<Option<LuaType>> {
    let Some(bindings) = bindings else {
        return Vec::new();
    };
    if !bindings
        .iter()
        .any(|b| matches!(b.state, FuncGenericBindingState::Unbound { .. }))
    {
        return Vec::new();
    }
    let slot_count = match types {
        VariadicType::Multi(types) => types.len(),
        VariadicType::Base(_) => 1,
    };
    (0..slot_count)
        .map(|idx| {
            let slot_type = match types {
                VariadicType::Multi(list) => list.get(idx),
                VariadicType::Base(base) => Some(base),
            };
            let want = slot_type.and_then(find_tpl_id_in_type);
            let want = want.unwrap_or(GenericTplId::Func(idx as u32));
            bindings.iter().find_map(|b| {
                if b.id != want {
                    return None;
                }
                match &b.state {
                    FuncGenericBindingState::Unbound { fallback } => Some(fallback.clone()),
                    FuncGenericBindingState::Bound => None,
                }
            })
        })
        .collect()
}

fn find_tpl_id_in_type(typ: &LuaType) -> Option<GenericTplId> {
    match typ {
        LuaType::TplRef(tpl) | LuaType::ConstTplRef(tpl) => Some(tpl.get_tpl_id()),
        LuaType::StrTplRef(tpl) => Some(tpl.get_tpl_id()),
        LuaType::Union(union) => union.types().find_map(find_tpl_id_in_type),
        LuaType::MultiLineUnion(union) => union
            .get_unions()
            .iter()
            .find_map(|(typ, _)| find_tpl_id_in_type(typ)),
        LuaType::Array(base) => find_tpl_id_in_type(base.get_base()),
        LuaType::TableOf(inner) => find_tpl_id_in_type(inner),
        _ => None,
    }
}

fn try_infer_pairs_iter_types_from_table_members(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    iter_expr: &LuaExpr,
    first_expr_type: &LuaType,
) -> Result<Option<VariadicType>, InferFailReason> {
    if !matches!(first_expr_type, LuaType::Variadic(_)) {
        return Ok(None);
    }

    let LuaExpr::CallExpr(call_expr) = iter_expr else {
        return Ok(None);
    };
    if !is_global_pairs_call(db, cache.get_file_id(), call_expr) {
        return Ok(None);
    }

    let Some(args_list) = call_expr.get_args_list() else {
        return Ok(None);
    };
    let mut args = args_list.get_args();
    let Some(table_arg) = args.next() else {
        return Ok(None);
    };
    if args.next().is_some() {
        return Ok(None);
    }

    let table_type = infer_expr(db, cache, table_arg)?;
    if matches!(table_type, LuaType::Global) {
        // The global table has no enumerable answer: its member types are the very
        // thing analysis is computing, and a loop over it can declare further
        // globals whose types are then part of the same union. Any snapshot is a
        // record of how far inference had progressed, not a fact about the program.
        return Ok(Some(VariadicType::Multi(vec![
            LuaType::String,
            LuaType::Any,
        ])));
    }
    if let LuaType::TableOf(inner) = &table_type {
        // Keep the value as T[K] instead of materializing every member type. Large
        // scripted-class hierarchies can contain hundreds of callable members.
        let key_type = infer_table_projection_key_type(db, inner);
        let value_type = LuaType::Call(
            LuaAliasCallType::new(
                LuaAliasCallKind::Index,
                vec![inner.as_ref().clone(), key_type.clone()],
            )
            .into(),
        );
        return Ok(Some(VariadicType::Multi(vec![key_type, value_type])));
    }
    let Some(table_type) = pairs_enumerable_type(db, table_type) else {
        return Ok(None);
    };
    let Some(members) = get_member_map(db, &table_type) else {
        return Ok(None);
    };
    if members.keys().any(is_pairs_metamethod_key) {
        return Ok(None);
    }
    let mut keys = Vec::new();
    let mut values = Vec::new();
    // Sort borrowed entries: the walk below only needs each member's `typ`
    // clone, so cloning the whole map's keys and info vectors up front would
    // pay a second deep clone of every member type for nothing.
    let mut member_entries = members
        .iter()
        .map(|(key, infos)| (key, infos, false))
        .collect::<Vec<_>>();
    member_entries.sort_by_key(|(key, _, _)| member_key_stable_key(key));

    // A dynamic key aliases every access whose key infers to the same type
    // rather than naming a member, so once one is present the literal keys
    // beside it are a sample of the indices some file happened to write, not
    // the table's key domain. Which samples are in the map depends on how far
    // inference had progressed, so the keys are reported by kind.
    //
    // A member filed by writing an iterated table back through a loop's own
    // key variable (`T[k] = v` inside `for k in pairs(T)`, in this loop or any
    // other over `T`; see `is_pairs_write_back` for how `T` is matched) is feedback,
    // not evidence: its key was minted from that loop variable's settling
    // answer, which is this very domain, so counting it would widen the domain
    // being derived. Such a member is excluded from the key domain — and from
    // the sampling decision — only when every writer under the key is one of
    // these write-backs and its key variable was not reassigned before the
    // write; a reassigned key or an independent writer under the same key is
    // real computed-key evidence.
    let mut self_write_only = |member_infos: &[LuaMemberInfo]| {
        !member_infos.is_empty()
            && member_infos.iter().all(|member| {
                matches!(
                    &member.property_owner_id,
                    Some(LuaSemanticDeclId::Member(member_id)) if is_pairs_write_back(db, cache, member_id)
                )
            })
    };
    for (_, member_infos, self_feedback) in &mut member_entries {
        *self_feedback = self_write_only(member_infos);
    }
    let keys_are_sampled = member_entries
        .iter()
        .any(|(key, _, self_feedback)| is_sampled_pairs_key(key) && !self_feedback);

    for (key, member_infos, self_feedback) in member_entries {
        if !self_feedback {
            let key_type = if keys_are_sampled {
                match table_projection_member_key_type(key) {
                    Some(typ) => typ,
                    None => continue,
                }
            } else {
                match key {
                    LuaMemberKey::Integer(i) => LuaType::IntegerConst(*i),
                    LuaMemberKey::Name(name) => LuaType::StringConst(name.clone().into()),
                    LuaMemberKey::ExprType(typ) => typ.clone(),
                    LuaMemberKey::None => continue,
                }
            };
            keys.push(key_type);
        }

        // Writing a loop's own value back (`T[k] = v` inside `for k, v in
        // pairs(T)`) puts back an element the table already holds, so that
        // value is feedback too: it carries whatever the element type was when
        // the write-back last ran. Any other value written is evidence.
        let evidence = member_infos
            .iter()
            .filter(|member| {
                !matches!(
                    &member.property_owner_id,
                    Some(LuaSemanticDeclId::Member(member_id)) if writes_back_loop_value(db, member_id)
                )
            })
            .collect::<Vec<_>>();
        let value_type = match evidence.as_slice() {
            [] if member_infos.is_empty() => LuaType::Any,
            [] => continue,
            [member] => member.typ.clone(),
            _ => LuaType::from_inferred_vec(
                evidence.iter().map(|member| member.typ.clone()).collect(),
            ),
        };
        values.push(value_type);
    }

    if keys.is_empty() || values.is_empty() {
        return Ok(None);
    }

    let iter_types = VariadicType::Multi(vec![
        compact_pairs_key_type(&keys),
        compact_pairs_value_type(db, values),
    ]);

    Ok(Some(iter_types))
}

/// The arms of `typ` that `pairs` enumerates: a string's members are the library its
/// metatable indexes, not entries of the value, so a string arm contributes none.
fn pairs_enumerable_type(db: &DbIndex, typ: LuaType) -> Option<LuaType> {
    let mut arms = Vec::new();
    collect_resolved_arms(db, typ, &mut arms, &mut HashSet::default());
    let enumerable = arms
        .into_iter()
        .filter(|arm| get_buildin_type_map_type_id(arm).is_none())
        .collect::<Vec<_>>();
    (!enumerable.is_empty()).then(|| LuaType::from_vec(enumerable))
}

/// The arms of `typ` with every alias among them resolved.
fn collect_resolved_arms(
    db: &DbIndex,
    typ: LuaType,
    arms: &mut Vec<LuaType>,
    visited: &mut HashSet<LuaType>,
) {
    if !visited.insert(typ.clone()) {
        return;
    }
    match crate::get_real_type(db, &typ).cloned().unwrap_or(typ) {
        LuaType::Union(union) => {
            for arm in union.types() {
                collect_resolved_arms(db, arm.clone(), arms, visited);
            }
        }
        LuaType::MultiLineUnion(union) => {
            for (arm, _) in union.get_unions() {
                collect_resolved_arms(db, arm.clone(), arms, visited);
            }
        }
        resolved => arms.push(resolved),
    }
}

/// Whether `member_id` writes a table back through the key variable of a
/// `pairs` loop over that same table, unreassigned before the write: a loop
/// over the table as the write spells it, or, in the file being analysed, one
/// every table of which is the table written. A write in another file is
/// judged by its spelling alone: inferring what its loop iterates would read
/// that file's text, so every edit of it would reach this file.
fn is_pairs_write_back(db: &DbIndex, cache: &mut LuaInferCache, member_id: &LuaMemberId) -> bool {
    let Some(index_expr) = written_index_expr(db, member_id) else {
        return false;
    };
    let Some(LuaIndexKey::Expr(LuaExpr::NameExpr(key_name))) = index_expr.get_index_key() else {
        return false;
    };
    if is_unreassigned_loop_var_over(db, member_id.file_id, &key_name, 0, &index_expr) {
        return true;
    }
    member_id.file_id == cache.get_file_id()
        && writes_own_pairs_key(db, cache, &index_expr).is_some()
}

/// Whether `member_id` writes the value variable of a `pairs` loop over the
/// same table back into it, unreassigned before the write.
fn writes_back_loop_value(db: &DbIndex, member_id: &LuaMemberId) -> bool {
    let Some(index_expr) = written_index_expr(db, member_id) else {
        return false;
    };
    let Some(assign_stat) = index_expr.get_parent::<LuaAssignStat>() else {
        return false;
    };
    let (vars, exprs) = assign_stat.get_var_and_expr_list();
    let written_value = vars
        .iter()
        .position(|var| var.syntax() == index_expr.syntax())
        .and_then(|at| exprs.get(at));
    let Some(LuaExpr::NameExpr(value_name)) = written_value else {
        return false;
    };
    is_unreassigned_loop_var_over(db, member_id.file_id, value_name, 1, &index_expr)
}

fn written_index_expr(db: &DbIndex, member_id: &LuaMemberId) -> Option<LuaIndexExpr> {
    let syntax_id = member_id.get_syntax_id();
    db.get_vfs()
        .syntax_tree_around(&member_id.file_id, syntax_id.get_range())
        .and_then(|tree| syntax_id.to_node_from_root(&tree.get_red_root()))
        .and_then(LuaIndexExpr::cast)
}

/// Whether `name` is variable `var_index` of a `pairs` loop over the table
/// `index_expr` writes into, and not reassigned before the write.
fn is_unreassigned_loop_var_over(
    db: &DbIndex,
    file_id: FileId,
    name: &LuaNameExpr,
    var_index: usize,
    index_expr: &LuaIndexExpr,
) -> bool {
    let Some(source) = for_range_pairs_source_for_var(db, file_id, name, var_index) else {
        return false;
    };
    let written_table = index_expr
        .get_prefix_expr()
        .and_then(|prefix| access_path(&prefix));
    written_table.is_some_and(|path| access_path(&source.source_expr) == Some(path))
        && db
            .get_reference_index()
            .first_local_reassignment(file_id, &source.iter_decl_id)
            .is_none_or(|position| position >= index_expr.get_position())
}

fn access_path(expr: &LuaExpr) -> Option<smol_str::SmolStr> {
    match expr {
        LuaExpr::NameExpr(name) => name.get_access_path(),
        LuaExpr::IndexExpr(index) => index.get_access_path(),
        _ => None,
    }
}

fn infer_table_projection_key_type(db: &DbIndex, inner: &LuaType) -> LuaType {
    let mut pending = vec![inner.clone()];
    let mut visited_types = HashSet::default();
    let mut key_type = None;

    while let Some(typ) = pending.pop() {
        match typ {
            LuaType::Ref(type_id) | LuaType::Def(type_id) => {
                if !visited_types.insert(type_id.clone()) {
                    continue;
                }

                let owner = LuaMemberOwner::Type(type_id.clone());
                // Iterate direct indexed keys and visit each supertype once;
                // flattened member discovery duplicates diamond inheritance paths.
                for key in db.get_member_index().get_member_keys(&owner) {
                    let Some(typ) = table_projection_member_key_type(key) else {
                        continue;
                    };
                    union_optional_type(db, &mut key_type, typ);
                }

                if let Some(super_types) = db.get_type_index().get_super_types_iter(&type_id) {
                    pending.extend(super_types.cloned());
                }
            }
            LuaType::Instance(_) => {
                let symbolic_key =
                    LuaType::Call(LuaAliasCallType::new(LuaAliasCallKind::KeyOf, vec![typ]).into());
                union_optional_type(db, &mut key_type, symbolic_key);
            }
            LuaType::Union(union) => pending.extend(union.types().cloned()),
            LuaType::MultiLineUnion(union) => pending.push(union.to_union()),
            LuaType::TableOf(inner) => pending.push(*inner),
            LuaType::TypeGuard(inner) => pending.push(inner.as_ref().clone()),
            typ => {
                let symbolic_key =
                    LuaType::Call(LuaAliasCallType::new(LuaAliasCallKind::KeyOf, vec![typ]).into());
                union_optional_type(db, &mut key_type, symbolic_key);
            }
        }
    }

    key_type.unwrap_or_else(|| {
        LuaType::Call(LuaAliasCallType::new(LuaAliasCallKind::KeyOf, vec![inner.clone()]).into())
    })
}

fn is_sampled_pairs_key(key: &LuaMemberKey) -> bool {
    match key {
        LuaMemberKey::ExprType(LuaType::Union(union))
            if !union.is_empty()
                && union.types().all(|typ| {
                    matches!(
                        typ,
                        LuaType::StringConst(_)
                            | LuaType::DocStringConst(_)
                            | LuaType::IntegerConst(_)
                            | LuaType::DocIntegerConst(_)
                    )
                }) =>
        {
            false
        }
        LuaMemberKey::ExprType(_) => true,
        _ => false,
    }
}

fn table_projection_member_key_type(key: &LuaMemberKey) -> Option<LuaType> {
    match key {
        LuaMemberKey::Name(_) => Some(LuaType::String),
        LuaMemberKey::Integer(_) => Some(LuaType::Integer),
        LuaMemberKey::ExprType(typ) => Some(match typ {
            LuaType::StringConst(_) | LuaType::DocStringConst(_) => LuaType::String,
            LuaType::IntegerConst(_) | LuaType::DocIntegerConst(_) => LuaType::Integer,
            LuaType::FloatConst(_) => LuaType::Number,
            typ => typ.clone(),
        }),
        LuaMemberKey::None => None,
    }
}

fn union_optional_type(db: &DbIndex, target: &mut Option<LuaType>, typ: LuaType) {
    *target = Some(match target.take() {
        Some(current) => TypeOps::Union.apply(db, &current, &typ),
        None => typ,
    });
}

const LARGE_EXACT_PAIRS_KEY_UNION_THRESHOLD: usize = 128;

fn compact_pairs_key_type(keys: &[LuaType]) -> LuaType {
    if keys.len() > LARGE_EXACT_PAIRS_KEY_UNION_THRESHOLD
        && keys
            .iter()
            .all(|key| matches!(key, LuaType::IntegerConst(_) | LuaType::DocIntegerConst(_)))
    {
        LuaType::Integer
    } else {
        LuaType::from_inferred_vec(keys.to_vec())
    }
}

fn compact_pairs_value_type(db: &DbIndex, values: Vec<LuaType>) -> LuaType {
    let mut saw_concrete = false;
    let mut values = values;
    values.retain_mut(|value| {
        *value = remove_pairs_yield_nil(db, value);
        if value.is_unknown() || value.is_never() {
            return false;
        }
        saw_concrete |= !value.is_any();
        true
    });
    if saw_concrete {
        values.retain(|value| !value.is_any());
    }
    if values.is_empty() {
        // All observed values were nil-only or otherwise uninformative; avoid collapsing to Nil.
        return LuaType::Unknown;
    }

    if values.len() == 1 && !matches!(values[0], LuaType::Union(_)) {
        return values.pop().unwrap_or(LuaType::Unknown);
    }

    try_compact_record_values(db, &values).unwrap_or_else(|| LuaType::from_inferred_vec(values))
}

fn remove_pairs_yield_nil(db: &DbIndex, value_type: &LuaType) -> LuaType {
    if matches!(value_type, LuaType::Unknown | LuaType::Never | LuaType::Nil) {
        return LuaType::Unknown;
    }
    if !matches!(
        value_type,
        LuaType::Union(_) | LuaType::Ref(_) | LuaType::Instance(_)
    ) {
        return value_type.clone();
    }

    let non_nil = TypeOps::Remove.apply(db, value_type, &LuaType::Nil);
    if non_nil.is_never() {
        LuaType::Unknown
    } else {
        non_nil
    }
}

fn try_compact_record_values(db: &DbIndex, values: &[LuaType]) -> Option<LuaType> {
    if values.len() < 2 || !values.iter().all(is_record_like_value_type) {
        return None;
    }

    let mut fields: HashMap<LuaMemberKey, (LuaType, usize)> = HashMap::default();
    for value in values {
        let Some(member_map) = get_member_map(db, value) else {
            continue;
        };
        let mut present_keys = HashSet::default();
        for (key, member_infos) in member_map.iter() {
            if matches!(key, LuaMemberKey::None) || member_infos.is_empty() {
                continue;
            }

            present_keys.insert(key.clone());
            for member in member_infos {
                fields
                    .entry(key.clone())
                    .and_modify(|(existing, _)| {
                        *existing = TypeOps::Union.apply(db, existing, &member.typ);
                    })
                    .or_insert((member.typ.clone(), 0));
            }
        }

        for key in present_keys {
            if let Some((_, present_count)) = fields.get_mut(&key) {
                *present_count += 1;
            }
        }
    }

    if fields.is_empty() {
        return None;
    }

    let total_values = values.len();
    let object_fields = fields
        .into_iter()
        .map(|(key, (typ, present_count))| {
            let typ = if present_count < total_values {
                TypeOps::Union.apply(db, &typ, &LuaType::Nil)
            } else {
                typ
            };
            (key, typ)
        })
        .collect();

    Some(LuaType::Object(
        LuaObjectType::new_with_fields(object_fields, Vec::new()).into(),
    ))
}

fn is_record_like_value_type(value: &LuaType) -> bool {
    matches!(
        value,
        LuaType::TableConst(_) | LuaType::Instance(_) | LuaType::Object(_)
    )
}

fn is_pairs_metamethod_key(key: &LuaMemberKey) -> bool {
    matches!(key, LuaMemberKey::Name(name) if name.as_str() == "__pairs")
}

fn member_key_stable_key(key: &LuaMemberKey) -> (u8, String) {
    match key {
        LuaMemberKey::None => (0, String::new()),
        LuaMemberKey::Integer(i) => (1, i.to_string()),
        LuaMemberKey::Name(name) => (2, name.to_string()),
        LuaMemberKey::ExprType(typ) => (3, format!("{typ:?}")),
    }
}

fn is_global_pairs_call(
    db: &DbIndex,
    file_id: FileId,
    call_expr: &glua_parser::LuaCallExpr,
) -> bool {
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

#[cfg(test)]
mod tests {
    use super::compact_pairs_value_type;
    use crate::{DbIndex, LuaType};

    #[test]
    fn compact_pairs_value_type_keeps_nil_and_any_cleanup_semantics() {
        let db = DbIndex::new();

        assert_eq!(
            compact_pairs_value_type(&db, vec![LuaType::Nil, LuaType::Unknown]),
            LuaType::Unknown
        );
        assert_eq!(
            compact_pairs_value_type(&db, vec![LuaType::Any, LuaType::String]),
            LuaType::String
        );
        assert_eq!(
            compact_pairs_value_type(&db, vec![LuaType::Integer, LuaType::Nil]),
            LuaType::Integer
        );
    }

    #[test]
    fn compact_pairs_value_type_matches_inferred_union_for_multiple_values() {
        let db = DbIndex::new();
        let values = vec![LuaType::Integer, LuaType::String];
        let expected = LuaType::from_inferred_vec(values.clone());

        assert_eq!(compact_pairs_value_type(&db, values), expected);
    }
}
