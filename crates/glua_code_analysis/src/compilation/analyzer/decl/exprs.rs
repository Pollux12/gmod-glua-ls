use glua_parser::{
    LuaAst, LuaAstNode, LuaAstToken, LuaBlock, LuaCallExpr, LuaCallExprStat, LuaClosureExpr,
    LuaDocTagCast, LuaExpr, LuaFuncStat, LuaIndexExpr, LuaIndexKey, LuaLiteralExpr,
    LuaLiteralToken, LuaNameExpr, LuaTableExpr, LuaVarExpr, NumberResult,
};
use smol_str::SmolStr;

use crate::{
    EmmyrcLuaVersion, FileId, InFiled, LegacyModuleEnv, LuaDeclExtra, LuaDeclId, LuaMemberFeature,
    LuaMemberId, LuaSignatureId, LuaType, LuaTypeCache, LuaTypeDeclId,
    db_index::{
        GmodScriptedClassCallKind, LuaDecl, LuaDependencyKind, LuaMember, LuaMemberKey,
        LuaMemberOwner,
    },
};

use super::{DeclAnalyzer, DeclOp};

pub fn analyze_name_expr(analyzer: &mut DeclAnalyzer, expr: LuaNameExpr) -> Option<()> {
    let name_token = expr.get_name_token()?;
    let name = name_token.get_name_text();
    let position = name_token.get_position();
    let range = name_token.get_range();
    let file_id = analyzer.get_file_id();
    let decl_id = LuaDeclId::new(file_id, position);
    let (decl_id, is_local) = if let Some(decl) = analyzer.decl.get_decl(&decl_id) {
        (Some(decl_id), decl.is_local())
    } else if let Some(decl) = analyzer.find_decl(name, position) {
        if decl.is_local() {
            // reference local variable
            (Some(decl.get_id()), true)
        } else {
            if decl.get_position() == position {
                return Some(());
            }
            // reference in filed global variable
            (Some(decl.get_id()), false)
        }
    } else {
        (None, false)
    };

    let is_scoped_class_global_name = analyzer.is_scoped_class_global_name(name);

    if let Some(id) = decl_id {
        analyzer.references.add_decl_reference(id, range, false);
    }

    if !is_local && !is_scoped_class_global_name {
        analyzer
            .references
            .add_global_reference(name, expr.get_syntax_id());
    }

    Some(())
}

pub fn analyze_index_expr(analyzer: &mut DeclAnalyzer, index_expr: LuaIndexExpr) -> Option<()> {
    if index_expr.ancestors::<LuaDocTagCast>().next().is_some() {
        return Some(());
    }
    let index_key = index_expr.get_index_key()?;
    let key = match index_key {
        LuaIndexKey::Name(name) => LuaMemberKey::Name(name.get_name_text().to_string().into()),
        LuaIndexKey::Integer(int) => {
            if let NumberResult::Int(i) = int.get_number_value() {
                LuaMemberKey::Integer(i)
            } else {
                return None;
            }
        }
        LuaIndexKey::String(string) => LuaMemberKey::Name(string.get_value().into()),
        LuaIndexKey::Expr(_) => return None,
        LuaIndexKey::Idx(i) => LuaMemberKey::Integer(i as i64),
    };

    let syntax_id = index_expr.get_syntax_id();
    let prefix = index_expr.get_prefix_expr()?;
    if let LuaExpr::NameExpr(name_expr) = prefix {
        let name_token = name_expr.get_name_token()?;
        let name_token_text = name_token.get_name_text();
        if name_token_text == "_G" || name_token_text == "_ENV" {
            if let LuaMemberKey::Name(name) = &key {
                analyzer.references.add_global_reference(name, syntax_id);
            }

            return Some(());
        }
    }

    analyzer.references.add_index_reference(key, syntax_id);

    Some(())
}

pub fn analyze_closure_expr(analyzer: &mut DeclAnalyzer, expr: LuaClosureExpr) -> Option<()> {
    let params = expr.get_params_list()?;
    let signature_id = LuaSignatureId::from_closure(analyzer.get_file_id(), &expr);
    let file_id = analyzer.get_file_id();
    let member_id = get_closure_member_id(&expr, file_id);
    if expr.get_parent::<glua_parser::LuaAssignStat>().is_some()
        && let Some(member_id) = member_id
    {
        analyzer
            .delta
            .early_callable_signatures
            .push((member_id.into(), signature_id));
    }
    try_add_self_param(analyzer, &expr);

    for (idx, param) in params.get_params().enumerate() {
        let name = param.get_name_token().map_or_else(
            || {
                if param.is_dots() {
                    "...".to_string()
                } else {
                    "".to_string()
                }
            },
            |name_token| name_token.get_name_text().to_string(),
        );
        let range = param.get_range();

        let decl = LuaDecl::new(
            &name,
            file_id,
            range,
            LuaDeclExtra::Param {
                idx,
                signature_id,
                owner_member_id: member_id,
            },
            None,
        );

        analyzer.add_decl(decl);
    }

    analyze_closure_params(analyzer, &signature_id, &expr);

    if is_structural_inferred_guard_candidate(&expr) {
        analyzer
            .delta
            .inferred_guard_candidates
            .push(InFiled::new(file_id, expr.get_syntax_id()));
    }

    Some(())
}

fn is_structural_inferred_guard_candidate(closure: &LuaClosureExpr) -> bool {
    let Some(params) = closure.get_params_list() else {
        return false;
    };
    let param_names = params
        .get_params()
        .filter_map(|param| param.get_name_token())
        .map(|token| token.get_name_text().to_string())
        .collect::<Vec<_>>();
    if param_names.is_empty() {
        return false;
    }
    let Some(block) = closure.get_block() else {
        return false;
    };
    let return_points = super::super::lua::func_body::analyze_func_body_returns(None, block);
    let mut truthy_exprs = Vec::new();
    for point in &return_points {
        match point {
            super::super::lua::LuaReturnPoint::Expr(expr)
                if !super::super::common::is_falsy_literal_expr(expr) =>
            {
                truthy_exprs.push(expr);
            }
            super::super::lua::LuaReturnPoint::MuliExpr(_) => return false,
            _ => {}
        }
    }
    if return_points.len() == 1 {
        return truthy_exprs
            .first()
            .is_some_and(|expr| inferred_guard_candidate_references_param(expr, &param_names));
    }
    !truthy_exprs.is_empty()
        && truthy_exprs.iter().all(|expr| {
            matches!(expr, LuaExpr::LiteralExpr(_))
                || inferred_guard_candidate_references_param(expr, &param_names)
        })
}

fn inferred_guard_candidate_references_param(expr: &LuaExpr, param_names: &[String]) -> bool {
    if let LuaExpr::ParenExpr(paren) = expr {
        return paren
            .get_expr()
            .is_some_and(|inner| inferred_guard_candidate_references_param(&inner, param_names));
    }
    matches!(expr, LuaExpr::BinaryExpr(_) | LuaExpr::CallExpr(_))
        && expr.descendants::<LuaNameExpr>().any(|name_expr| {
            name_expr
                .get_name_text()
                .is_some_and(|name| param_names.iter().any(|param| *param == name))
        })
}

fn try_add_self_param(analyzer: &mut DeclAnalyzer, closure: &LuaClosureExpr) -> Option<()> {
    let func_stat = closure.get_parent::<LuaFuncStat>()?;
    let func_name = func_stat.get_func_name()?;
    let LuaVarExpr::IndexExpr(index_expr) = func_name else {
        return Some(());
    };

    let index_token = index_expr.get_index_token()?;
    if !index_token.is_colon() {
        return Some(());
    }

    let self_param = LuaDecl::new(
        "self",
        analyzer.get_file_id(),
        index_token.get_range(),
        LuaDeclExtra::ImplicitSelf {
            kind: index_token.syntax().kind(),
        },
        None,
    );

    analyzer.add_decl(self_param);
    Some(())
}

fn get_closure_member_id(closure: &LuaClosureExpr, file_id: FileId) -> Option<LuaMemberId> {
    let parent = closure.get_parent::<LuaAst>()?;
    match parent {
        LuaAst::LuaAssignStat(assign) => {
            let (vars, value_exprs) = assign.get_var_and_expr_list();
            let value_idx = value_exprs
                .iter()
                .position(|expr| expr.get_position() == closure.get_position())?;
            let var = vars.get(value_idx)?;
            if let LuaVarExpr::IndexExpr(index_expr) = var {
                return Some(LuaMemberId::new(index_expr.get_syntax_id(), file_id));
            }
        }
        LuaAst::LuaFuncStat(func_stat) => {
            let func_name = func_stat.get_func_name()?;
            if let LuaVarExpr::IndexExpr(index_expr) = func_name {
                return Some(LuaMemberId::new(index_expr.get_syntax_id(), file_id));
            }
        }
        LuaAst::LuaTableField(table_field) => {
            return Some(LuaMemberId::new(table_field.get_syntax_id(), file_id));
        }
        _ => {}
    }

    None
}

fn analyze_closure_params(
    analyzer: &mut DeclAnalyzer,
    signature_id: &LuaSignatureId,
    closure: &LuaClosureExpr,
) -> Option<()> {
    let is_colon_define = closure
        .get_parent::<LuaFuncStat>()
        .and_then(|func_stat| func_stat.get_func_name())
        .is_some_and(|name| match name {
            LuaVarExpr::IndexExpr(index_expr) => index_expr
                .get_index_token()
                .is_some_and(|token| token.is_colon()),
            _ => false,
        });
    let mut signature = DeclOp::Signature {
        id: *signature_id,
        is_colon_define,
        params: Vec::new(),
        is_vararg: None,
    };
    let finished = closure_params(closure, &mut signature);
    analyzer.ops.push(signature);
    finished
}

fn closure_params(closure: &LuaClosureExpr, signature: &mut DeclOp) -> Option<()> {
    let DeclOp::Signature {
        params, is_vararg, ..
    } = signature
    else {
        return None;
    };
    let mut has_dots = false;
    for param in closure.get_params_list()?.get_params() {
        if param.is_dots() {
            has_dots = true;
        }

        let name = if let Some(name_token) = param.get_name_token() {
            SmolStr::new(name_token.get_name_text())
        } else if param.is_dots() {
            SmolStr::new_static("...")
        } else {
            return None;
        };

        params.push(name);
    }

    *is_vararg = Some(has_dots);

    Some(())
}

pub fn analyze_table_expr(analyzer: &mut DeclAnalyzer, table_expr: LuaTableExpr) -> Option<()> {
    // Register members for keyed/object tables AND for shaped sequential
    // literals (arrays whose rows are themselves table literals). The latter are
    // inferred as TableConst (see `infer_table_expr`), so their integer-keyed
    // members (`[1]`, `[2]`, ...) must be registered here for `t[1]` lookups and
    // rich hover. Simple scalar arrays are summarized as `T[]` and need no
    // members. The predicate is purely syntactic so this declaration pass and
    // the inference pass agree on which literals are materialized.
    if table_expr.is_object() || table_expr.is_shaped_array_literal() {
        let file_id = analyzer.get_file_id();
        let owner_id = LuaMemberOwner::Element(InFiled {
            file_id,
            value: table_expr.get_range(),
        });
        let decl_feature = if analyzer.is_meta {
            LuaMemberFeature::MetaDefine
        } else {
            LuaMemberFeature::FileDefine
        };

        for field in table_expr.get_fields() {
            if let Some(field_key) = field.get_field_key() {
                let key: LuaMemberKey = match field_key {
                    LuaIndexKey::Name(name) => LuaMemberKey::Name(name.get_name_text().into()),
                    LuaIndexKey::String(str) => LuaMemberKey::Name(str.get_value().into()),
                    LuaIndexKey::Integer(i) => {
                        if let NumberResult::Int(idx) = i.get_number_value() {
                            LuaMemberKey::Integer(idx)
                        } else {
                            continue;
                        }
                    }
                    LuaIndexKey::Idx(idx) => LuaMemberKey::Integer(idx as i64),
                    LuaIndexKey::Expr(_) => continue,
                };

                analyzer
                    .references
                    .add_index_reference(key.clone(), field.get_syntax_id());

                let member_id = LuaMemberId::new(field.get_syntax_id(), file_id);
                let member = match &owner_id {
                    LuaMemberOwner::GlobalPath(path) => {
                        LuaMember::new(member_id, key, decl_feature, Some(path.clone()))
                    }
                    _ => LuaMember::new(member_id, key, decl_feature, None),
                };
                analyzer.add_member(owner_id.clone(), member);
            }
        }
    }

    Some(())
}

pub fn analyze_literal_expr(analyzer: &mut DeclAnalyzer, expr: LuaLiteralExpr) -> Option<()> {
    let literal = expr.get_literal()?;
    let file_id = analyzer.get_file_id();

    match literal {
        LuaLiteralToken::String(string_token) => {
            if !analyzer.db.get_emmyrc().references.short_string_search {
                return Some(());
            }

            let value = string_token.get_value();
            if value.len() <= 64 {
                analyzer
                    .references
                    .add_string_reference(&value, string_token.get_range());
            }
        }
        LuaLiteralToken::Dots(dots_token) => {
            let position = dots_token.get_position();
            let range = dots_token.get_range();

            let decl_id = LuaDeclId::new(file_id, position);
            let decl_id = analyzer
                .decl
                .get_decl(&decl_id)
                .map(|_| decl_id)
                .or_else(|| {
                    analyzer
                        .find_decl(dots_token.get_text(), position)
                        .and_then(|decl| decl.is_local().then(|| decl.get_id()))
                });

            if let Some(id) = decl_id {
                analyzer.references.add_decl_reference(id, range, false);
            }
        }
        _ => {}
    }

    Some(())
}

pub fn analyze_call_expr(analyzer: &mut DeclAnalyzer, expr: LuaCallExpr) -> Option<()> {
    try_analyze_legacy_module_call(analyzer, &expr);
    try_declare_define_baseclass_local(analyzer, &expr);

    let Some((dependency_kind, dependency_path)) = get_dependency_call_info(&expr) else {
        return Some(());
    };

    let file_id = analyzer.get_file_id();

    // No-arg AddCSLuaFile() means "send self to client": a self-reference.
    let self_reference = dependency_path.is_none()
        && dependency_kind == LuaDependencyKind::AddCSLuaFile
        && dependency_call_has_no_args(&expr);
    let dependency_path_keys = dependency_path
        .as_deref()
        .map(|path| crate::dependency_site_path_keys(analyzer.db, file_id, path))
        .unwrap_or_default();

    let call_range = expr.get_range();
    let range = expr
        .get_args_list()
        .and_then(|args| args.get_args().next())
        .map_or(call_range, |arg| arg.get_range());
    analyzer.ops.push(DeclOp::Dependency {
        kind: dependency_kind,
        path: dependency_path,
        self_reference,
        path_keys: dependency_path_keys,
        original_expr: expr.syntax().text().to_string(),
        call_range,
        range,
    });

    Some(())
}

/// `DEFINE_BASECLASS("X")` expands to `local BaseClass = baseclass.Get("X")`,
/// so it declares a local at the call, typed by the class it names.
fn try_declare_define_baseclass_local(
    analyzer: &mut DeclAnalyzer,
    expr: &LuaCallExpr,
) -> Option<()> {
    let LuaExpr::NameExpr(name_expr) = expr.get_prefix_expr()? else {
        return Some(());
    };
    if !analyzer.db.get_emmyrc().gmod.enabled
        || GmodScriptedClassCallKind::from_call_name(&name_expr.get_name_text()?)
            != Some(GmodScriptedClassCallKind::DefineBaseClass)
        || expr.get_parent::<LuaCallExprStat>().is_none()
    {
        return Some(());
    }

    let args_list = expr.get_args_list()?;
    let decl_id = analyzer.add_decl(LuaDecl::new(
        "BaseClass",
        analyzer.get_file_id(),
        args_list.get_range(),
        LuaDeclExtra::Local {
            kind: args_list.syntax().kind(),
            attrib: None,
        },
        None,
    ));
    let base_type = match get_static_string_arg(expr) {
        Some(base_name) if !base_name.is_empty() => LuaType::Ref(LuaTypeDeclId::global(&base_name)),
        _ => LuaType::Any,
    };
    analyzer.ops.push(DeclOp::WriteTypeCache {
        owner: decl_id.into(),
        cache: LuaTypeCache::DocType(base_type),
    });
    Some(())
}

fn try_analyze_legacy_module_call(analyzer: &mut DeclAnalyzer, expr: &LuaCallExpr) -> Option<()> {
    let runtime_version = analyzer.db.get_emmyrc().runtime.version;
    if !matches!(
        runtime_version,
        EmmyrcLuaVersion::Lua51 | EmmyrcLuaVersion::LuaJIT
    ) {
        return Some(());
    }

    let Some(call_name) = get_call_name(expr) else {
        return Some(());
    };
    if call_name != "module" {
        return Some(());
    }

    let Some(call_stat) = expr.get_parent::<LuaCallExprStat>() else {
        return Some(());
    };
    let Some(parent_block) = call_stat.get_parent::<LuaBlock>() else {
        return Some(());
    };
    let Some(LuaAst::LuaChunk(_)) = parent_block.get_parent::<LuaAst>() else {
        return Some(());
    };

    let args = expr.get_args_list()?;
    let mut args_iter = args.get_args();
    let module_path = match args_iter.next()? {
        LuaExpr::LiteralExpr(literal_expr) => match literal_expr.get_literal()? {
            LuaLiteralToken::String(str) => str.get_value(),
            _ => return Some(()),
        },
        _ => return Some(()),
    };

    // Any option argument makes the module see all globals.
    let seeall = args_iter.next().is_some();
    let call_range = expr.get_range();
    let activation_position = call_range.end();

    analyzer.set_legacy_module_env(LegacyModuleEnv {
        module_path: module_path.clone(),
        activation_position,
        seeall,
    });

    // Synthesize a global decl for the module's top-level chain segment so that
    // references to the module's own name (e.g. `tc` inside `module("tc", ...)`,
    // or `tc` from any other file) resolve cleanly: no undefined-global
    // diagnostic, goto-definition jumps to the `module(...)` call, and hover
    // shows the module table.
    //
    // The decl points at the `module(...)` call (not the `"tc"` string literal)
    // so goto-def lands on the module declaration statement itself — analogous
    // to a Lua 5.1 module's "definition site". Member goto-def (e.g. on
    // `tc.checkTable`) is unaffected and continues to resolve to the actual
    // function definition via the existing namespace member resolver.
    //
    // Type inference for these names is special-cased in `infer_global_type` to
    // return `LuaType::Namespace(module_path)` so member access (e.g. `tc.foo`)
    // resolves through the legacy module namespace, not through this synthetic
    // decl's type.
    if let Some(top_segment) = module_path.split('.').next()
        && !top_segment.is_empty()
    {
        let kind = glua_parser::LuaSyntaxKind::NameExpr.into();
        let synthetic_decl = LuaDecl::new(
            top_segment,
            analyzer.get_file_id(),
            call_range,
            LuaDeclExtra::Global { kind },
            None,
        );
        analyzer.add_decl(synthetic_decl);
    }

    Some(())
}

fn get_dependency_call_info(expr: &LuaCallExpr) -> Option<(LuaDependencyKind, Option<String>)> {
    let dependency_kind = if expr.is_require() {
        LuaDependencyKind::Require
    } else {
        match get_call_name(expr).as_deref()? {
            "include" => LuaDependencyKind::Include,
            "AddCSLuaFile" => LuaDependencyKind::AddCSLuaFile,
            "IncludeCS" => LuaDependencyKind::IncludeCS,
            _ => return None,
        }
    };
    let dependency_path = get_static_string_arg(expr);
    if dependency_path.is_none() && dependency_kind == LuaDependencyKind::Require {
        return None;
    }
    Some((dependency_kind, dependency_path))
}

fn dependency_call_has_no_args(expr: &LuaCallExpr) -> bool {
    expr.get_args_list()
        .is_none_or(|args| args.get_args().next().is_none())
}

fn get_call_name(expr: &LuaCallExpr) -> Option<String> {
    match expr.get_prefix_expr()? {
        LuaExpr::NameExpr(name_expr) => name_expr.get_name_text().map(Into::into),
        LuaExpr::IndexExpr(index_expr) => match index_expr.get_index_key()? {
            LuaIndexKey::Name(name) => Some(name.get_name_text().to_string()),
            LuaIndexKey::String(string) => Some(string.get_value()),
            _ => None,
        },
        _ => None,
    }
}

fn get_static_string_arg(expr: &LuaCallExpr) -> Option<String> {
    let args = expr.get_args_list()?;
    let first_arg = args.get_args().next()?;
    let LuaExpr::LiteralExpr(literal_expr) = first_arg else {
        return None;
    };
    let LuaLiteralToken::String(string_token) = literal_expr.get_literal()? else {
        return None;
    };
    Some(string_token.get_value())
}
