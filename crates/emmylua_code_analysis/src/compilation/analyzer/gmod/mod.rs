use std::{collections::{HashMap, HashSet}, sync::Arc};

use emmylua_parser::{
    LuaAssignStat, LuaAst, LuaAstNode, LuaAstToken, LuaCallExpr, LuaChunk, LuaComment,
    LuaCommentOwner,
    LuaDocDescriptionOwner,
    LuaDocTag, LuaDocTagRealm, LuaExpr, LuaFuncStat, LuaIfStat, LuaIndexKey, LuaLiteralToken,
    LuaVarExpr, PathTrait,
};

use crate::{
    EmmyrcGmodRealm, FileId, GmodClassCallLiteral, GmodScriptedClassCallMetadata,
    LuaDeclTypeKind, LuaFunctionType, LuaMember, LuaMemberFeature, LuaMemberId, LuaMemberKey,
    LuaType, LuaTypeCache, LuaTypeDecl, LuaTypeDeclId, LuaTypeFlag,
    compilation::analyzer::{
        AnalysisPipeline, AnalyzeContext, common::add_member,
    },
    db_index::{
        AsyncState, DbIndex, GmodCallbackSiteMetadata, GmodConVarKind, GmodConVarSiteMetadata,
        GmodConcommandSiteMetadata, GmodHookKind, GmodHookNameIssue, GmodHookSiteMetadata,
        GmodNamedSiteMetadata, GmodNetReceiveSiteMetadata, GmodRealm, GmodRealmFileMetadata,
        GmodRealmRange, GmodTimerKind, GmodTimerSiteMetadata, LuaDependencyKind, LuaMemberOwner,
    },
    profile::Profile,
};

pub struct GmodAnalysisPipeline;

impl AnalysisPipeline for GmodAnalysisPipeline {
    fn analyze(db: &mut DbIndex, context: &mut AnalyzeContext) {
        if !db.get_emmyrc().gmod.enabled {
            return;
        }

        let _p = Profile::cond_new("gmod analyze", context.tree_list.len() > 1);
        let tree_list = context.tree_list.clone();
        let file_ids: Vec<FileId> = tree_list.iter().map(|x| x.file_id).collect();

        // Pre-compute scripted class scope for all files (compile globs once)
        let scripted_scope_files = context.get_or_compute_scripted_scope_files(db).clone();

        let mut branch_realm_ranges: HashMap<FileId, Vec<GmodRealmRange>> = HashMap::new();
        let mut annotation_realms: HashMap<FileId, GmodRealm> = HashMap::new();
        for in_filed_tree in &tree_list {
            let is_in_scope = scripted_scope_files.contains(&in_filed_tree.file_id);
            collect_hook_metadata(db, in_filed_tree.file_id, in_filed_tree.value.clone());
            if is_in_scope {
                if let Some(scope_match) = detect_scoped_class_from_path(db, in_filed_tree.file_id) {
                    ensure_scoped_class_type_decl(
                        db,
                        in_filed_tree.file_id,
                        &scope_match.class_name,
                        scope_match.global_name,
                        in_filed_tree.value.syntax().text_range(),
                    );
                }
                collect_scripted_scope_type_bindings(db, in_filed_tree.file_id);
                synthesize_scoped_base_assignments(db, in_filed_tree.file_id, in_filed_tree.value.clone());
            }
            let ranges = collect_branch_realm_ranges(&in_filed_tree.value);
            if !ranges.is_empty() {
                branch_realm_ranges.insert(in_filed_tree.file_id, ranges);
            }
            if let Some(realm) =
                collect_realm_annotation(&in_filed_tree.value)
            {
                annotation_realms.insert(in_filed_tree.file_id, realm);
            }
        }

        synthesize_scripted_class_members(db, &scripted_scope_files, &file_ids);
        synthesize_vgui_registrations(db, &file_ids);

        rebuild_realm_metadata(db, branch_realm_ranges, annotation_realms);
    }
}

fn collect_hook_metadata(db: &mut DbIndex, file_id: FileId, root: LuaChunk) {
    for call_expr in root.descendants::<LuaCallExpr>() {
        if let Some(site) = collect_hook_call_site(db, call_expr.clone()) {
            db.get_gmod_infer_index_mut().add_hook_site(file_id, site);
        }

        collect_system_call_metadata(db, file_id, call_expr);
    }

    for func_stat in root.descendants::<LuaFuncStat>() {
        if let Some(site) = collect_hook_method_site(db, func_stat) {
            db.get_gmod_infer_index_mut().add_hook_site(file_id, site);
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct GmodScopedGlobalRule {
    global_name: &'static str,
    folder_segments: &'static [&'static str],
}

#[derive(Debug, Clone)]
struct GmodScopedClassMatch {
    global_name: &'static str,
    class_name: String,
}

const GMOD_SCOPED_GLOBAL_RULES: &[GmodScopedGlobalRule] = &[
    GmodScopedGlobalRule {
        global_name: "TOOL",
        folder_segments: &["weapons", "gmod_tool", "stools"],
    },
    GmodScopedGlobalRule {
        global_name: "ENT",
        folder_segments: &["entities"],
    },
    GmodScopedGlobalRule {
        global_name: "SWEP",
        folder_segments: &["weapons"],
    },
    GmodScopedGlobalRule {
        global_name: "EFFECT",
        folder_segments: &["effects"],
    },
    GmodScopedGlobalRule {
        global_name: "PLUGIN",
        folder_segments: &["plugins"],
    },
];

const GMOD_ENT_BASE_TO_ENT: &[&str] = &[
    "base_gmodentity",
    "base_brush",
    "base_anim",
    "base_ai",
    "base_nextbot",
    "base_point",
    "base_filter",
];

fn collect_scripted_scope_type_bindings(db: &mut DbIndex, file_id: FileId) {
    let Some(scope_match) = detect_scoped_class_from_path(db, file_id) else {
        return;
    };

    let mut decls = Vec::new();
    {
        let Some(decl_tree) = db.get_decl_index().get_decl_tree(&file_id) else {
            return;
        };

        for decl in decl_tree.get_decls().values() {
            if decl.get_name() != scope_match.global_name {
                continue;
            }

            if decl.is_local() || decl.is_global() {
                decls.push((decl.get_id(), decl.get_range()));
            }
        }
    }

    if decls.is_empty() {
        return;
    }

    let class_decl_id = ensure_scoped_class_type_decl(
        db,
        file_id,
        &scope_match.class_name,
        scope_match.global_name,
        decls[0].1,
    );

    for (decl_id, _) in decls {
        let previous_decl_type = db
            .get_type_index()
            .get_type_cache(&decl_id.into())
            .map(|type_cache| type_cache.as_type().clone());

        db.get_type_index_mut().force_bind_type(
            decl_id.into(),
            LuaTypeCache::InferType(LuaType::Def(class_decl_id.clone())),
        );

        if let Some(LuaType::TableConst(table_range)) = previous_decl_type {
            let table_member_owner = LuaMemberOwner::Element(table_range);
            let class_member_owner = LuaMemberOwner::Type(class_decl_id.clone());
            let table_member_ids = db
                .get_member_index()
                .get_members(&table_member_owner)
                .map(|members| members.iter().map(|member| member.get_id()).collect::<Vec<_>>())
                .unwrap_or_default();
            for member_id in table_member_ids {
                add_member(db, class_member_owner.clone(), member_id);
            }
        }
    }
}

fn ensure_scoped_class_type_decl(
    db: &mut DbIndex,
    file_id: FileId,
    class_name: &str,
    global_name: &str,
    range: rowan::TextRange,
) -> LuaTypeDeclId {
    let class_decl_id = LuaTypeDeclId::global(class_name);
    if db.get_type_index().get_type_decl(&class_decl_id).is_none() {
        db.get_type_index_mut().add_type_decl(
            file_id,
            LuaTypeDecl::new(
                file_id,
                range,
                class_decl_id.get_simple_name().to_string(),
                LuaDeclTypeKind::Class,
                LuaTypeFlag::None.into(),
                class_decl_id.clone(),
            ),
        );
    }

    for super_type in scoped_class_super_types(global_name) {
        let has_super = db
            .get_type_index()
            .get_super_types_iter(&class_decl_id)
            .map(|mut supers| supers.any(|existing_super| existing_super == &super_type))
            .unwrap_or(false);
        if !has_super {
            db.get_type_index_mut()
                .add_super_type(class_decl_id.clone(), file_id, super_type);
        }
    }

    class_decl_id
}

fn scoped_class_super_types(global_name: &str) -> Vec<LuaType> {
    let mut super_types = vec![LuaType::Ref(LuaTypeDeclId::global(global_name))];
    if global_name == "PLUGIN" {
        super_types.push(LuaType::Ref(LuaTypeDeclId::global("GM")));
    }

    super_types
}

pub(crate) fn ensure_scoped_class_type_decl_for_file(
    db: &mut DbIndex,
    file_id: FileId,
    range: rowan::TextRange,
) -> Option<LuaTypeDeclId> {
    let scope_match = detect_scoped_class_from_path(db, file_id)?;
    Some(ensure_scoped_class_type_decl(
        db,
        file_id,
        &scope_match.class_name,
        scope_match.global_name,
        range,
    ))
}

/// Synthesize typed members from AccessorFunc, NetworkVar, and DEFINE_BASECLASS
/// calls for all files that have scripted class metadata.
fn synthesize_scripted_class_members(
    db: &mut DbIndex,
    scripted_scope_files: &HashSet<FileId>,
    file_ids: &[FileId],
) {
    for file_id in file_ids.iter().copied() {
        let scope_match = if scripted_scope_files.contains(&file_id) {
            detect_scoped_class_from_path(db, file_id)
        } else {
            None
        };

        let metadata = match db
            .get_gmod_class_metadata_index()
            .get_file_metadata(&file_id)
        {
            Some(m) => m.clone(),
            None => continue,
        };

        // DEFINE_BASECLASS: set super type on the scoped class
        if let Some(ref scope_match) = scope_match {
            let class_decl_id = LuaTypeDeclId::global(&scope_match.class_name);
            for call in &metadata.define_baseclass_calls {
                synthesize_define_baseclass(db, file_id, &class_decl_id, call);
            }
        }

        // AccessorFunc: synthesize Get/Set/field members
        if let Some(ref scope_match) = scope_match {
            let class_decl_id = LuaTypeDeclId::global(&scope_match.class_name);
            for call in &metadata.accessor_func_calls {
                synthesize_accessor_func(db, file_id, &class_decl_id, call);
            }
        }

        // NetworkVar: synthesize Get/Set members
        if let Some(ref scope_match) = scope_match {
            let class_decl_id = LuaTypeDeclId::global(&scope_match.class_name);
            for call in &metadata.network_var_calls {
                synthesize_network_var(db, file_id, &class_decl_id, call);
            }
        }
    }
}

/// Synthesize vgui.Register / derma.DefineControl class types.
fn synthesize_vgui_registrations(db: &mut DbIndex, file_ids: &[FileId]) {
    for file_id in file_ids.iter().copied() {
        let metadata = match db
            .get_gmod_class_metadata_index()
            .get_file_metadata(&file_id)
        {
            Some(m) => m.clone(),
            None => continue,
        };

        for call in &metadata.vgui_register_calls {
            synthesize_vgui_register(db, file_id, call);
        }

        for call in &metadata.derma_define_control_calls {
            synthesize_derma_define_control(db, file_id, call);
        }
    }
}

fn synthesize_scoped_base_assignments(db: &mut DbIndex, file_id: FileId, root: LuaChunk) {
    let Some(scope_match) = detect_scoped_class_from_path(db, file_id) else {
        return;
    };

    let class_decl_id = ensure_scoped_class_type_decl(
        db,
        file_id,
        &scope_match.class_name,
        scope_match.global_name,
        root.syntax().text_range(),
    );
    let expected_base_path = format!("{}.Base", scope_match.global_name);

    for assign_stat in root.descendants::<LuaAssignStat>() {
        let (vars, exprs) = assign_stat.get_var_and_expr_list();
        for (idx, var) in vars.into_iter().enumerate() {
            let Some(value_expr) = exprs.get(idx) else {
                continue;
            };

            let Some(access_path) = var.get_access_path() else {
                continue;
            };
            if !access_path.eq_ignore_ascii_case(&expected_base_path) {
                continue;
            }

            let Some(base_name) = extract_scoped_base_name(value_expr) else {
                continue;
            };

            let mapped_base_name = remap_scoped_base_name(&scope_match, &base_name);
            let super_type = LuaType::Ref(LuaTypeDeclId::global(&mapped_base_name));
            if super_type == LuaType::Ref(class_decl_id.clone()) {
                continue;
            }

            let has_super = db
                .get_type_index()
                .get_super_types_iter(&class_decl_id)
                .map(|mut supers| supers.any(|existing_super| existing_super == &super_type))
                .unwrap_or(false);
            if !has_super {
                db.get_type_index_mut()
                    .add_super_type(class_decl_id.clone(), file_id, super_type);
            }
        }
    }
}

fn extract_scoped_base_name(expr: &LuaExpr) -> Option<String> {
    match expr {
        LuaExpr::LiteralExpr(literal_expr) => match literal_expr.get_literal() {
            Some(LuaLiteralToken::String(string_token)) => {
                let value = string_token.get_value();
                (!value.trim().is_empty()).then_some(value)
            }
            _ => None,
        },
        LuaExpr::NameExpr(name_expr) => {
            let value = name_expr.get_name_text()?;
            (!value.trim().is_empty()).then_some(value)
        }
        LuaExpr::IndexExpr(index_expr) => {
            let value = index_expr.get_access_path()?;
            (!value.trim().is_empty()).then_some(value)
        }
        _ => None,
    }
}

fn remap_scoped_base_name(scope_match: &GmodScopedClassMatch, base_name: &str) -> String {
    if scope_match.global_name == "ENT"
        && GMOD_ENT_BASE_TO_ENT
            .iter()
            .any(|name| name.eq_ignore_ascii_case(base_name))
    {
        return scope_match.global_name.to_string();
    }

    base_name.to_string()
}

fn synthesize_define_baseclass(
    db: &mut DbIndex,
    file_id: FileId,
    class_decl_id: &LuaTypeDeclId,
    call: &GmodScriptedClassCallMetadata,
) {
    // DEFINE_BASECLASS("base_name") → set super type
    let base_name = match call.literal_args.first() {
        Some(Some(GmodClassCallLiteral::String(name))) => name.clone(),
        _ => return,
    };

    if base_name.is_empty() {
        return;
    }

    let super_type = LuaType::Ref(LuaTypeDeclId::global(&base_name));
    let has_super = db
        .get_type_index()
        .get_super_types_iter(class_decl_id)
        .map(|mut supers| supers.any(|existing_super| existing_super == &super_type))
        .unwrap_or(false);
    if !has_super {
        db.get_type_index_mut()
            .add_super_type(class_decl_id.clone(), file_id, super_type);
    }
}

fn synthesize_accessor_func(
    db: &mut DbIndex,
    file_id: FileId,
    class_decl_id: &LuaTypeDeclId,
    call: &GmodScriptedClassCallMetadata,
) {
    // AccessorFunc(target, "m_VarKey", "Name", forceType)
    // args[0] = target (ENT etc) - non-literal name ref
    // args[1] = backing field name (string)
    // args[2] = accessor name (string)
    // args[3] = force type (FORCE_STRING, number, bool, etc)

    let accessor_name = match call.literal_args.get(2) {
        Some(Some(GmodClassCallLiteral::String(name))) => name.clone(),
        _ => return,
    };

    if accessor_name.is_empty() {
        return;
    }

    let var_key = match call.literal_args.get(1) {
        Some(Some(GmodClassCallLiteral::String(name))) => Some(name.clone()),
        _ => None,
    };

    let force_type = call.literal_args.get(3).and_then(|arg| arg.as_ref());
    let value_type = resolve_accessor_force_type(force_type);
    let self_type = LuaType::Ref(class_decl_id.clone());
    let owner = LuaMemberOwner::Type(class_decl_id.clone());

    // Synthesize the backing field if var_key is present
    if let Some(ref var_key_name) = var_key {
        if let Some(field_syntax_id) = call.args.get(1).map(|a| a.syntax_id) {
            let member_id = LuaMemberId::new(field_syntax_id, file_id);
            let member = LuaMember::new(
                member_id,
                LuaMemberKey::Name(var_key_name.as_str().into()),
                LuaMemberFeature::FileFieldDecl,
                None,
            );
            db.get_member_index_mut()
                .add_member(owner.clone(), member);
            db.get_type_index_mut().bind_type(
                member_id.into(),
                LuaTypeCache::DocType(value_type.clone()),
            );
        }
    }

    // Synthesize the getter: GetName(self: Class): valueType
    if let Some(getter_syntax_id) = call.args.get(2).map(|a| a.syntax_id) {
        let getter_name = format!("Get{accessor_name}");
        let getter_func = LuaFunctionType::new(
            AsyncState::None,
            true,
            false,
            vec![("self".to_string(), Some(self_type.clone()))],
            value_type.clone(),
        );
        let member_id = LuaMemberId::new(getter_syntax_id, file_id);
        let member = LuaMember::new(
            member_id,
            LuaMemberKey::Name(getter_name.as_str().into()),
            LuaMemberFeature::FileMethodDecl,
            None,
        );
        db.get_member_index_mut()
            .add_member(owner.clone(), member);
        db.get_type_index_mut().bind_type(
            member_id.into(),
            LuaTypeCache::DocType(LuaType::DocFunction(Arc::new(getter_func))),
        );
    }

    // Synthesize the setter: SetName(self: Class, value: valueType)
    let setter_syntax_id = call.syntax_id;
    let setter_name = format!("Set{accessor_name}");
    let setter_func = LuaFunctionType::new(
        AsyncState::None,
        true,
        false,
        vec![
            ("self".to_string(), Some(self_type)),
            ("value".to_string(), Some(value_type)),
        ],
        LuaType::Nil,
    );
    let member_id = LuaMemberId::new(setter_syntax_id, file_id);
    let member = LuaMember::new(
        member_id,
        LuaMemberKey::Name(setter_name.as_str().into()),
        LuaMemberFeature::FileMethodDecl,
        None,
    );
    db.get_member_index_mut()
        .add_member(owner.clone(), member);
    db.get_type_index_mut().bind_type(
        member_id.into(),
        LuaTypeCache::DocType(LuaType::DocFunction(Arc::new(setter_func))),
    );
}

fn synthesize_network_var(
    db: &mut DbIndex,
    file_id: FileId,
    class_decl_id: &LuaTypeDeclId,
    call: &GmodScriptedClassCallMetadata,
) {
    // ENT:NetworkVar("Type", slot, "Name") or
    // ENT:NetworkVarElement("Type", slot, element, "Name")
    // args[0] = type name (string like "Float", "String", "Bool", etc)
    // args[1] = slot (integer)
    // args[2] = property name (string)
    // For NetworkVarElement: args[2] = element, args[3] = name

    let type_name = match call.literal_args.first() {
        Some(Some(GmodClassCallLiteral::String(name))) => name.clone(),
        _ => return,
    };

    // Try 3rd arg first (standard NetworkVar), then 4th (NetworkVarElement)
    let (prop_name, prop_name_arg_idx) = match call.literal_args.get(2) {
        Some(Some(GmodClassCallLiteral::String(name))) if !name.is_empty() => (name.clone(), 2usize),
        _ => match call.literal_args.get(3) {
            Some(Some(GmodClassCallLiteral::String(name))) if !name.is_empty() => {
                (name.clone(), 3usize)
            }
            _ => return,
        },
    };

    let value_type = resolve_networkvar_type(&type_name);
    let self_type = LuaType::Ref(class_decl_id.clone());
    let owner = LuaMemberOwner::Type(class_decl_id.clone());

    // Synthesize getter: GetPropName(self: Class): valueType
    if let Some(getter_syntax_id) = call.args.get(prop_name_arg_idx).map(|a| a.syntax_id) {
        let getter_name = format!("Get{prop_name}");
        let getter_func = LuaFunctionType::new(
            AsyncState::None,
            true,
            false,
            vec![("self".to_string(), Some(self_type.clone()))],
            value_type.clone(),
        );
        let member_id = LuaMemberId::new(getter_syntax_id, file_id);
        let member = LuaMember::new(
            member_id,
            LuaMemberKey::Name(getter_name.as_str().into()),
            LuaMemberFeature::FileMethodDecl,
            None,
        );
        db.get_member_index_mut()
            .add_member(owner.clone(), member);
        db.get_type_index_mut().bind_type(
            member_id.into(),
            LuaTypeCache::DocType(LuaType::DocFunction(Arc::new(getter_func))),
        );
    }

    // Synthesize setter: SetPropName(self: Class, value: valueType)
    let setter_syntax_id = call.syntax_id;
    let setter_name = format!("Set{prop_name}");
    let setter_func = LuaFunctionType::new(
        AsyncState::None,
        true,
        false,
        vec![
            ("self".to_string(), Some(self_type)),
            ("value".to_string(), Some(value_type)),
        ],
        LuaType::Nil,
    );
    let member_id = LuaMemberId::new(setter_syntax_id, file_id);
    let member = LuaMember::new(
        member_id,
        LuaMemberKey::Name(setter_name.as_str().into()),
        LuaMemberFeature::FileMethodDecl,
        None,
    );
    db.get_member_index_mut()
        .add_member(owner.clone(), member);
    db.get_type_index_mut().bind_type(
        member_id.into(),
        LuaTypeCache::DocType(LuaType::DocFunction(Arc::new(setter_func))),
    );
}

fn synthesize_vgui_register(
    db: &mut DbIndex,
    file_id: FileId,
    call: &GmodScriptedClassCallMetadata,
) {
    // vgui.Register("PanelName", TABLE, "BasePanel")
    // args[0] = panel name (string)
    // args[1] = table variable (name ref)
    // args[2] = base panel name (string)
    let panel_name = match call.literal_args.first() {
        Some(Some(GmodClassCallLiteral::String(name))) if !name.is_empty() => name.clone(),
        _ => return,
    };

    let table_var_name = match call.literal_args.get(1) {
        Some(Some(GmodClassCallLiteral::NameRef(name))) => Some(name.clone()),
        _ => None,
    };

    let base_panel = match call.literal_args.get(2) {
        Some(Some(GmodClassCallLiteral::String(name))) if !name.is_empty() => Some(name.clone()),
        _ => None,
    };

    synthesize_panel_class(db, file_id, &panel_name, table_var_name.as_deref(), base_panel.as_deref(), call);
}

fn synthesize_derma_define_control(
    db: &mut DbIndex,
    file_id: FileId,
    call: &GmodScriptedClassCallMetadata,
) {
    // derma.DefineControl("ControlName", "description", TABLE, "BasePanel")
    // args[0] = control name (string)
    // args[1] = description (string, ignored)
    // args[2] = table variable (name ref)
    // args[3] = base panel name (string)
    let control_name = match call.literal_args.first() {
        Some(Some(GmodClassCallLiteral::String(name))) if !name.is_empty() => name.clone(),
        _ => return,
    };

    let table_var_name = match call.literal_args.get(2) {
        Some(Some(GmodClassCallLiteral::NameRef(name))) => Some(name.clone()),
        _ => None,
    };

    let base_panel = match call.literal_args.get(3) {
        Some(Some(GmodClassCallLiteral::String(name))) if !name.is_empty() => Some(name.clone()),
        _ => None,
    };

    synthesize_panel_class(db, file_id, &control_name, table_var_name.as_deref(), base_panel.as_deref(), call);
}

fn synthesize_panel_class(
    db: &mut DbIndex,
    file_id: FileId,
    panel_name: &str,
    table_var_name: Option<&str>,
    base_panel: Option<&str>,
    call: &GmodScriptedClassCallMetadata,
) {
    let class_decl_id = LuaTypeDeclId::global(panel_name);

    // Create the class type declaration if it doesn't exist
    if db.get_type_index().get_type_decl(&class_decl_id).is_none() {
        db.get_type_index_mut().add_type_decl(
            file_id,
            LuaTypeDecl::new(
                file_id,
                call.syntax_id.get_range(),
                class_decl_id.get_simple_name().to_string(),
                LuaDeclTypeKind::Class,
                LuaTypeFlag::None.into(),
                class_decl_id.clone(),
            ),
        );
    }

    // Set super type from base panel
    if let Some(base_name) = base_panel {
        let super_type = LuaType::Ref(LuaTypeDeclId::global(base_name));
        let has_super = db
            .get_type_index()
            .get_super_types_iter(&class_decl_id)
            .map(|mut supers| supers.any(|existing_super| existing_super == &super_type))
            .unwrap_or(false);
        if !has_super {
            db.get_type_index_mut()
                .add_super_type(class_decl_id.clone(), file_id, super_type);
        }
    }

    // Bind the table variable to the panel class
    if let Some(var_name) = table_var_name {
        let Some(decl_tree) = db.get_decl_index().get_decl_tree(&file_id) else {
            return;
        };

        let matching_decls: Vec<_> = decl_tree
            .get_decls()
            .values()
            .filter(|decl| decl.get_name() == var_name)
            .map(|decl| (decl.get_id(), decl.get_range()))
            .collect();

        for (decl_id, _) in &matching_decls {
            let previous_decl_type = db
                .get_type_index()
                .get_type_cache(&(*decl_id).into())
                .map(|type_cache| type_cache.as_type().clone());

            db.get_type_index_mut().force_bind_type(
                (*decl_id).into(),
                LuaTypeCache::InferType(LuaType::Def(class_decl_id.clone())),
            );

            // Transfer table members to the class
            if let Some(LuaType::TableConst(table_range)) = previous_decl_type {
                let table_member_owner = LuaMemberOwner::Element(table_range);
                let class_member_owner = LuaMemberOwner::Type(class_decl_id.clone());
                let table_member_ids = db
                    .get_member_index()
                    .get_members(&table_member_owner)
                    .map(|members| {
                        members.iter().map(|member| member.get_id()).collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                for member_id in table_member_ids {
                    add_member(db, class_member_owner.clone(), member_id);
                }
            }
        }
    }
}

/// Resolve AccessorFunc force type argument to a LuaType.
fn resolve_accessor_force_type(force_arg: Option<&GmodClassCallLiteral>) -> LuaType {
    match force_arg {
        Some(GmodClassCallLiteral::NameRef(name)) => match name.as_str() {
            "FORCE_STRING" => LuaType::String,
            "FORCE_NUMBER" => LuaType::Number,
            "FORCE_BOOL" => LuaType::Boolean,
            "FORCE_ANGLE" => LuaType::Ref(LuaTypeDeclId::global("Angle")),
            "FORCE_COLOR" => LuaType::Ref(LuaTypeDeclId::global("Color")),
            "FORCE_VECTOR" => LuaType::Ref(LuaTypeDeclId::global("Vector")),
            _ => LuaType::Any,
        },
        Some(GmodClassCallLiteral::Integer(n)) => match *n {
            1 => LuaType::String,
            2 => LuaType::Number,
            3 => LuaType::Boolean,
            4 => LuaType::Ref(LuaTypeDeclId::global("Angle")),
            5 => LuaType::Ref(LuaTypeDeclId::global("Color")),
            6 => LuaType::Ref(LuaTypeDeclId::global("Vector")),
            _ => LuaType::Any,
        },
        Some(GmodClassCallLiteral::Unsigned(n)) => match *n {
            1 => LuaType::String,
            2 => LuaType::Number,
            3 => LuaType::Boolean,
            4 => LuaType::Ref(LuaTypeDeclId::global("Angle")),
            5 => LuaType::Ref(LuaTypeDeclId::global("Color")),
            6 => LuaType::Ref(LuaTypeDeclId::global("Vector")),
            _ => LuaType::Any,
        },
        Some(GmodClassCallLiteral::Boolean(true)) => LuaType::Boolean,
        _ => LuaType::Any,
    }
}

/// Resolve NetworkVar type name to a LuaType.
fn resolve_networkvar_type(type_name: &str) -> LuaType {
    match type_name {
        "String" => LuaType::String,
        "Bool" => LuaType::Boolean,
        "Float" | "Double" => LuaType::Number,
        "Int" | "UInt" => LuaType::Integer,
        "Vector" => LuaType::Ref(LuaTypeDeclId::global("Vector")),
        "Angle" => LuaType::Ref(LuaTypeDeclId::global("Angle")),
        "Entity" => LuaType::Ref(LuaTypeDeclId::global("Entity")),
        "Color" => LuaType::Ref(LuaTypeDeclId::global("Color")),
        _ => {
            log::warn!(
                "Unknown NetworkVar type '{}', defaulting to Any. Valid types are: \
                String, Bool, Float, Double, Int, UInt, Vector, Angle, Entity, Color",
                type_name
            );
            LuaType::Any
        }
    }
}

fn detect_scoped_class_from_path(db: &DbIndex, file_id: FileId) -> Option<GmodScopedClassMatch> {
    let file_path = db.get_vfs().get_file_path(&file_id)?;
    let normalized_path = file_path.to_string_lossy().replace('\\', "/");
    let lower_segments = normalized_path
        .to_ascii_lowercase()
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    if lower_segments.is_empty() {
        return None;
    }

    let mut best_match: Option<(&GmodScopedGlobalRule, usize, usize)> = None;

    for rule in GMOD_SCOPED_GLOBAL_RULES {
        let rule_len = rule.folder_segments.len();
        if rule_len == 0 || lower_segments.len() < rule_len {
            continue;
        }

        for start_idx in (0..=lower_segments.len() - rule_len).rev() {
            let mut matched = true;
            for (offset, rule_segment) in rule.folder_segments.iter().enumerate() {
                if lower_segments[start_idx + offset] != *rule_segment {
                    matched = false;
                    break;
                }
            }

            if !matched {
                continue;
            }

            let end_idx = start_idx + rule_len - 1;
            let replace_best = match best_match {
                None => true,
                Some((_, best_end_idx, best_rule_len)) => {
                    end_idx > best_end_idx || (end_idx == best_end_idx && rule_len > best_rule_len)
                }
            };

            if replace_best {
                best_match = Some((rule, end_idx, rule_len));
            }

            break;
        }
    }

    let (rule, best_end_idx, _) = best_match?;
    let class_idx = best_end_idx + 1;
    if class_idx >= lower_segments.len() {
        return None;
    }

    let class_name = if class_idx == lower_segments.len() - 1 {
        lower_segments[class_idx]
            .strip_suffix(".lua")
            .unwrap_or(lower_segments[class_idx].as_str())
            .to_string()
    } else {
        lower_segments[class_idx].clone()
    };

    if class_name.is_empty() {
        return None;
    }

    Some(GmodScopedClassMatch {
        global_name: rule.global_name,
        class_name,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GmodSystemCallKind {
    AddNetworkString,
    NetStart,
    NetReceive,
    ConcommandAdd,
    CreateConVar,
    CreateClientConVar,
    TimerCreate,
    TimerSimple,
}

fn collect_system_call_metadata(
    db: &mut DbIndex,
    file_id: FileId,
    call_expr: LuaCallExpr,
) -> Option<()> {
    let call_path = call_expr.get_access_path()?;
    let kind = classify_system_call_path(&call_path)?;

    match kind {
        GmodSystemCallKind::AddNetworkString => {
            let (name, name_range) = extract_static_string_arg(call_expr.clone(), 0);
            db.get_gmod_infer_index_mut().add_net_message_registration(
                file_id,
                GmodNamedSiteMetadata {
                    syntax_id: call_expr.get_syntax_id(),
                    name,
                    name_range,
                },
            );
        }
        GmodSystemCallKind::NetStart => {
            let (name, name_range) = extract_static_string_arg(call_expr.clone(), 0);
            db.get_gmod_infer_index_mut().add_net_start_site(
                file_id,
                GmodNamedSiteMetadata {
                    syntax_id: call_expr.get_syntax_id(),
                    name,
                    name_range,
                },
            );
        }
        GmodSystemCallKind::NetReceive => {
            let (message_name, name_range) = extract_static_string_arg(call_expr.clone(), 0);
            let callback = extract_callback_arg(call_expr.clone(), 1);
            db.get_gmod_infer_index_mut().add_net_receive_site(
                file_id,
                GmodNetReceiveSiteMetadata {
                    syntax_id: call_expr.get_syntax_id(),
                    message_name,
                    name_range,
                    callback,
                },
            );
        }
        GmodSystemCallKind::ConcommandAdd => {
            let (command_name, name_range) = extract_static_string_arg(call_expr.clone(), 0);
            let callback = extract_callback_arg(call_expr.clone(), 1);
            db.get_gmod_infer_index_mut().add_concommand_site(
                file_id,
                GmodConcommandSiteMetadata {
                    syntax_id: call_expr.get_syntax_id(),
                    command_name,
                    name_range,
                    callback,
                },
            );
        }
        GmodSystemCallKind::CreateConVar | GmodSystemCallKind::CreateClientConVar => {
            let (convar_name, name_range) = extract_static_string_arg(call_expr.clone(), 0);
            db.get_gmod_infer_index_mut().add_convar_site(
                file_id,
                GmodConVarSiteMetadata {
                    syntax_id: call_expr.get_syntax_id(),
                    kind: if kind == GmodSystemCallKind::CreateClientConVar {
                        GmodConVarKind::Client
                    } else {
                        GmodConVarKind::Server
                    },
                    convar_name,
                    name_range,
                },
            );
        }
        GmodSystemCallKind::TimerCreate => {
            let (timer_name, name_range) = extract_static_string_arg(call_expr.clone(), 0);
            let callback = extract_callback_arg(call_expr.clone(), 3);
            db.get_gmod_infer_index_mut().add_timer_site(
                file_id,
                GmodTimerSiteMetadata {
                    syntax_id: call_expr.get_syntax_id(),
                    kind: GmodTimerKind::Create,
                    timer_name,
                    name_range,
                    callback,
                },
            );
        }
        GmodSystemCallKind::TimerSimple => {
            let callback = extract_callback_arg(call_expr.clone(), 1);
            db.get_gmod_infer_index_mut().add_timer_site(
                file_id,
                GmodTimerSiteMetadata {
                    syntax_id: call_expr.get_syntax_id(),
                    kind: GmodTimerKind::Simple,
                    timer_name: None,
                    name_range: None,
                    callback,
                },
            );
        }
    }

    Some(())
}

fn classify_system_call_path(path: &str) -> Option<GmodSystemCallKind> {
    if matches_call_path(path, "util.AddNetworkString") {
        return Some(GmodSystemCallKind::AddNetworkString);
    }
    if matches_call_path(path, "net.Start") {
        return Some(GmodSystemCallKind::NetStart);
    }
    if matches_call_path(path, "net.Receive") {
        return Some(GmodSystemCallKind::NetReceive);
    }
    if matches_call_path(path, "concommand.Add") {
        return Some(GmodSystemCallKind::ConcommandAdd);
    }
    if matches_call_path(path, "CreateClientConVar") {
        return Some(GmodSystemCallKind::CreateClientConVar);
    }
    if matches_call_path(path, "CreateConVar") {
        return Some(GmodSystemCallKind::CreateConVar);
    }
    if matches_call_path(path, "timer.Create") {
        return Some(GmodSystemCallKind::TimerCreate);
    }
    if matches_call_path(path, "timer.Simple") {
        return Some(GmodSystemCallKind::TimerSimple);
    }
    None
}

fn matches_call_path(path: &str, target: &str) -> bool {
    path == target || path.ends_with(&format!(".{target}")) || path.ends_with(&format!(":{target}"))
}

fn extract_static_string_arg(
    call_expr: LuaCallExpr,
    arg_idx: usize,
) -> (Option<String>, Option<rowan::TextRange>) {
    let Some(arg_expr) = call_expr
        .get_args_list()
        .and_then(|args| args.get_args().nth(arg_idx))
    else {
        return (None, None);
    };

    let LuaExpr::LiteralExpr(literal_expr) = arg_expr else {
        return (None, None);
    };

    match literal_expr.get_literal() {
        Some(LuaLiteralToken::String(string_token)) => (
            Some(string_token.get_value()),
            Some(string_token.get_range()),
        ),
        Some(_) => (None, Some(literal_expr.get_range())),
        None => (None, Some(literal_expr.get_range())),
    }
}

fn extract_callback_arg(call_expr: LuaCallExpr, arg_idx: usize) -> GmodCallbackSiteMetadata {
    let Some(callback_expr) = call_expr
        .get_args_list()
        .and_then(|args| args.get_args().nth(arg_idx))
    else {
        return GmodCallbackSiteMetadata::default();
    };

    GmodCallbackSiteMetadata {
        syntax_id: Some(callback_expr.get_syntax_id()),
        callback_range: Some(callback_expr.get_range()),
    }
}

fn collect_hook_call_site(db: &DbIndex, call_expr: LuaCallExpr) -> Option<GmodHookSiteMetadata> {
    let call_path = call_expr.get_access_path()?;
    let mapped_hook = mapped_hook_for_emitter_call(db, &call_path, call_expr.clone());
    let kind = mapped_hook
        .as_ref()
        .map(|_| GmodHookKind::Emit)
        .or_else(|| classify_hook_call_path(&call_path))?;
    let (hook_name, name_range, name_issue) = mapped_hook.unwrap_or_else(|| {
        extract_static_hook_name(
            call_expr
                .get_args_list()
                .and_then(|args| args.get_args().next()),
        )
    });

    Some(GmodHookSiteMetadata {
        syntax_id: call_expr.get_syntax_id(),
        kind,
        hook_name,
        name_range,
        name_issue,
        callback_params: if kind == GmodHookKind::Add {
            extract_hook_callback_params_from_call(&call_expr)
        } else {
            Vec::new()
        },
    })
}

fn classify_hook_call_path(path: &str) -> Option<GmodHookKind> {
    if matches_call_path(path, "hook.Add") {
        return Some(GmodHookKind::Add);
    }

    if matches_call_path(path, "hook.Run") || matches_call_path(path, "hook.Call") {
        return Some(GmodHookKind::Emit);
    }

    None
}

fn mapped_hook_for_emitter_call(
    db: &DbIndex,
    call_path: &str,
    call_expr: LuaCallExpr,
) -> Option<(
    Option<String>,
    Option<rowan::TextRange>,
    Option<GmodHookNameIssue>,
)> {
    for (emitter_path, mapped_hook) in &db.get_emmyrc().gmod.hook_mappings.emitter_to_hook {
        if !matches_call_path(call_path, emitter_path) {
            continue;
        }

        if mapped_hook == "*" {
            return Some(extract_static_hook_name(
                call_expr
                    .get_args_list()
                    .and_then(|args| args.get_args().next()),
            ));
        }

        let trimmed = mapped_hook.trim();
        return Some(if trimmed.is_empty() {
            (None, None, Some(GmodHookNameIssue::Empty))
        } else {
            (Some(trimmed.to_string()), None, None)
        });
    }

    None
}

fn collect_hook_method_site(db: &DbIndex, func_stat: LuaFuncStat) -> Option<GmodHookSiteMetadata> {
    let LuaVarExpr::IndexExpr(index_expr) = func_stat.get_func_name()? else {
        return None;
    };
    let is_colon = index_expr.get_index_token()?.is_colon();

    let LuaExpr::NameExpr(prefix_name_expr) = index_expr.get_prefix_expr()? else {
        return None;
    };

    let prefix_name = prefix_name_expr.get_name_text()?;
    let separator = if is_colon { ":" } else { "." };

    let (method_name, name_range) = match index_expr.get_index_key()? {
        LuaIndexKey::Name(name_token) => (
            Some(name_token.get_name_text().to_string()),
            Some(name_token.get_range()),
        ),
        LuaIndexKey::String(string_token) => (
            Some(string_token.get_value()),
            Some(string_token.get_range()),
        ),
        _ => (None, None),
    };

    let mapped_method_hook = method_mapped_hook_name(
        db,
        &prefix_name,
        separator,
        method_name.as_deref().unwrap_or_default(),
    );
    let annotation = hook_annotation_from_doc(&func_stat);
    let trimmed_method_name = method_name
        .as_ref()
        .map(|name| name.trim().to_string())
        .unwrap_or_default();
    let (hook_name, mut name_issue) = if let Some((hook_name, name_issue)) = mapped_method_hook {
        (hook_name, name_issue)
    } else if let Some(annotation_hook) = annotation {
        let hook_name = annotation_hook.hook_name.or_else(|| {
            (!trimmed_method_name.is_empty()).then_some(trimmed_method_name.to_string())
        });
        let name_issue = if hook_name.is_none() {
            Some(GmodHookNameIssue::Empty)
        } else {
            annotation_hook.name_issue
        };
        (hook_name, name_issue)
    } else {
        if !is_colon
            || (!is_builtin_method_hook_prefix(&prefix_name)
                && !is_configured_method_hook_prefix(db, &prefix_name))
        {
            return None;
        }
        let name_issue = trimmed_method_name
            .is_empty()
            .then_some(GmodHookNameIssue::Empty);
        let hook_name = (!trimmed_method_name.is_empty()).then_some(trimmed_method_name);
        (hook_name, name_issue)
    };

    let hook_name = normalize_gamemode_hook_name(hook_name);
    if hook_name.is_none() && name_issue.is_none() {
        name_issue = Some(GmodHookNameIssue::Empty);
    }

    Some(GmodHookSiteMetadata {
        syntax_id: index_expr.get_syntax_id(),
        kind: GmodHookKind::GamemodeMethod,
        hook_name,
        name_range,
        name_issue,
        callback_params: extract_hook_callback_params_from_method(&func_stat),
    })
}

fn extract_hook_callback_params_from_call(call_expr: &LuaCallExpr) -> Vec<String> {
    let Some(callback_expr) = call_expr
        .get_args_list()
        .and_then(|args| args.get_args().nth(2))
    else {
        return Vec::new();
    };

    let LuaExpr::ClosureExpr(closure_expr) = callback_expr else {
        return Vec::new();
    };

    extract_param_names_from_closure(closure_expr)
}

fn extract_hook_callback_params_from_method(func_stat: &LuaFuncStat) -> Vec<String> {
    let Some(closure_expr) = func_stat.get_closure() else {
        return Vec::new();
    };

    extract_param_names_from_closure(closure_expr)
}

fn extract_param_names_from_closure(closure_expr: emmylua_parser::LuaClosureExpr) -> Vec<String> {
    let Some(params_list) = closure_expr.get_params_list() else {
        return Vec::new();
    };

    params_list
        .get_params()
        .filter_map(|param| {
            if param.is_dots() {
                Some("...".to_string())
            } else {
                Some(param.get_name_token()?.get_name_text().to_string())
            }
        })
        .collect()
}

fn is_builtin_method_hook_prefix(prefix_name: &str) -> bool {
    matches!(prefix_name, "GM" | "GAMEMODE" | "PLUGIN" | "SANDBOX")
}

fn is_configured_method_hook_prefix(db: &DbIndex, prefix_name: &str) -> bool {
    db.get_emmyrc()
        .gmod
        .hook_mappings
        .method_prefixes
        .iter()
        .any(|configured_prefix| {
            configured_prefix
                .trim()
                .trim_end_matches([':', '.'])
                .eq_ignore_ascii_case(prefix_name)
        })
}

#[derive(Debug, Clone)]
struct HookAnnotationMetadata {
    hook_name: Option<String>,
    name_issue: Option<GmodHookNameIssue>,
}

fn hook_annotation_from_doc(func_stat: &LuaFuncStat) -> Option<HookAnnotationMetadata> {
    let comment = func_stat.get_left_comment()?;
    for tag in comment.get_doc_tags() {
        let LuaDocTag::Other(other_tag) = tag else {
            continue;
        };
        let tag_name = other_tag.get_tag_name()?;
        if !tag_name
            .trim_start_matches('@')
            .eq_ignore_ascii_case("hook")
        {
            continue;
        }

        let annotation_value = other_tag
            .get_description()
            .map(|description| description.get_description_text())
            .unwrap_or_default();
        let normalized = annotation_value.trim();
        let hook_name = (!normalized.is_empty()).then_some(normalized.to_string());

        return Some(HookAnnotationMetadata {
            hook_name,
            name_issue: None,
        });
    }

    None
}

fn method_mapped_hook_name(
    db: &DbIndex,
    prefix_name: &str,
    separator: &str,
    method_name: &str,
) -> Option<(Option<String>, Option<GmodHookNameIssue>)> {
    let mappings = &db.get_emmyrc().gmod.hook_mappings.method_to_hook;
    let method_name = method_name.trim();
    let mut candidates = vec![format!("{prefix_name}{separator}{method_name}")];
    if separator == ":" {
        candidates.push(format!("{prefix_name}.{method_name}"));
    } else {
        candidates.push(format!("{prefix_name}:{method_name}"));
    }

    for candidate in candidates {
        let Some(mapped_hook) = mappings.get(&candidate) else {
            continue;
        };

        if mapped_hook == "*" {
            return Some((
                (!method_name.is_empty()).then_some(method_name.to_string()),
                method_name.is_empty().then_some(GmodHookNameIssue::Empty),
            ));
        }

        let trimmed = mapped_hook.trim();
        return Some(if trimmed.is_empty() {
            (None, Some(GmodHookNameIssue::Empty))
        } else {
            (Some(trimmed.to_string()), None)
        });
    }

    None
}

fn normalize_gamemode_hook_name(hook_name: Option<String>) -> Option<String> {
    let hook_name = hook_name?;
    let trimmed = hook_name.trim();
    if trimmed.is_empty() {
        return None;
    }

    let normalized = strip_builtin_method_hook_prefix(trimmed)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(trimmed);

    Some(normalized.to_string())
}

fn strip_builtin_method_hook_prefix(name: &str) -> Option<&str> {
    for separator in [':', '.'] {
        let Some((prefix, remainder)) = name.split_once(separator) else {
            continue;
        };

        if is_builtin_method_hook_prefix(prefix.trim()) {
            return Some(remainder);
        }
    }

    None
}

fn extract_static_hook_name(
    first_arg: Option<LuaExpr>,
) -> (
    Option<String>,
    Option<rowan::TextRange>,
    Option<GmodHookNameIssue>,
) {
    let Some(first_arg) = first_arg else {
        return (None, None, None);
    };

    let LuaExpr::LiteralExpr(literal_expr) = first_arg else {
        return (None, None, None);
    };

    match literal_expr.get_literal() {
        Some(LuaLiteralToken::String(string_token)) => {
            let hook_name = string_token.get_value();
            let issue = hook_name
                .trim()
                .is_empty()
                .then_some(GmodHookNameIssue::Empty);
            (Some(hook_name), Some(string_token.get_range()), issue)
        }
        Some(_) => (
            None,
            Some(literal_expr.get_range()),
            Some(GmodHookNameIssue::NonStringLiteral),
        ),
        None => (
            None,
            Some(literal_expr.get_range()),
            Some(GmodHookNameIssue::NonStringLiteral),
        ),
    }
}

/// Detect `if CLIENT then`/`if SERVER then` blocks and return realm-narrowed ranges.
fn collect_branch_realm_ranges(root: &LuaChunk) -> Vec<GmodRealmRange> {
    let mut ranges = Vec::new();
    for if_stat in root.descendants::<LuaIfStat>() {
        collect_if_realm_ranges(&if_stat, &mut ranges);
    }
    ranges.sort_by_key(|range| (range.range.len(), range.range.start()));
    ranges
}

/// Collect the first `---@realm client|server|shared` annotation from a file.
fn collect_realm_annotation(root: &LuaChunk) -> Option<GmodRealm> {
    for comment in root.descendants::<LuaComment>() {
        let is_file_level = matches!(comment.get_owner(), None | Some(LuaAst::LuaChunk(_)));
        if !is_file_level {
            continue;
        }

        for tag in comment.get_doc_tags() {
            if let LuaDocTag::Realm(realm_tag) = tag
                && let Some(realm) = realm_from_doc_tag(&realm_tag)
            {
                return Some(realm);
            }
        }
    }

    None
}

fn realm_from_doc_tag(tag: &LuaDocTagRealm) -> Option<GmodRealm> {
    let name = tag.get_name_token()?;
    match name.get_name_text() {
        "client" => Some(GmodRealm::Client),
        "server" => Some(GmodRealm::Server),
        "shared" => Some(GmodRealm::Shared),
        _ => None,
    }
}

/// Extract realm narrowing from a single if-statement, handling if/elseif/else clauses.
fn collect_if_realm_ranges(if_stat: &LuaIfStat, ranges: &mut Vec<GmodRealmRange>) {
    let condition_realm = if_stat
        .get_condition_expr()
        .as_ref()
        .and_then(realm_from_condition);

    if let Some(realm) = condition_realm {
        if let Some(block) = if_stat.get_block() {
            let range = block.syntax().text_range();
            ranges.push(GmodRealmRange { range, realm });
        }

        // Identify the complementary realm for else block
        let complement = match realm {
            GmodRealm::Client => Some(GmodRealm::Server),
            GmodRealm::Server => Some(GmodRealm::Client),
            _ => None,
        };

        // Handle elseif/else clauses
        let mut has_elseif = false;
        for clause in if_stat.get_all_clause() {
            match &clause {
                emmylua_parser::LuaIfClauseStat::ElseIf(elseif) => {
                    has_elseif = true;
                    if let Some(elseif_realm) =
                        elseif.get_condition_expr().as_ref().and_then(realm_from_condition)
                    {
                        if let Some(block) = elseif.get_block() {
                            ranges.push(GmodRealmRange {
                                range: block.syntax().text_range(),
                                realm: elseif_realm,
                            });
                        }
                    }
                }
                emmylua_parser::LuaIfClauseStat::Else(else_clause) => {
                    // Only assign complement realm if there's no elseif
                    // (with elseif, else block realm is ambiguous)
                    if !has_elseif {
                        if let Some(complement_realm) = complement {
                            if let Some(block) = else_clause.get_block() {
                                ranges.push(GmodRealmRange {
                                    range: block.syntax().text_range(),
                                    realm: complement_realm,
                                });
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Match condition expressions to realms.
/// Handles: `CLIENT`, `SERVER`, `not CLIENT`, `not SERVER`
fn realm_from_condition(expr: &LuaExpr) -> Option<GmodRealm> {
    match expr {
        LuaExpr::NameExpr(name_expr) => match name_expr.get_name_text()?.as_str() {
            "CLIENT" => Some(GmodRealm::Client),
            "SERVER" => Some(GmodRealm::Server),
            _ => None,
        },
        LuaExpr::UnaryExpr(unary_expr) => {
            let op = unary_expr.get_op_token()?;
            let op_kind = op.get_op();
            if op_kind == emmylua_parser::UnaryOperator::OpNot {
                let inner = unary_expr.get_expr()?;
                let inner_realm = realm_from_condition(&inner)?;
                match inner_realm {
                    GmodRealm::Client => Some(GmodRealm::Server),
                    GmodRealm::Server => Some(GmodRealm::Client),
                    _ => None,
                }
            } else {
                None
            }
        }
        _ => None,
    }
}

fn rebuild_realm_metadata(
    db: &mut DbIndex,
    branch_realm_ranges: HashMap<FileId, Vec<GmodRealmRange>>,
    annotation_realms: HashMap<FileId, GmodRealm>,
) {
    let file_ids = db.get_vfs().get_all_local_file_ids();
    let default_realm = gmod_config_default_realm(db);
    let detect_filename = db
        .get_emmyrc()
        .gmod
        .detect_realm_from_filename
        .unwrap_or(true);
    let detect_calls = db.get_emmyrc().gmod.detect_realm_from_calls.unwrap_or(true);
    if !detect_filename && !detect_calls {
        let realm_metadata = file_ids
            .into_iter()
            .map(|file_id| {
                let ranges = branch_realm_ranges
                    .get(&file_id)
                    .cloned()
                    .unwrap_or_default();
                let annotation_realm = annotation_realms.get(&file_id).copied();
                let realm = annotation_realm.unwrap_or(default_realm);
                (
                    file_id,
                    GmodRealmFileMetadata {
                        inferred_realm: realm,
                        annotation_realm,
                        branch_realm_ranges: ranges,
                        ..Default::default()
                    },
                )
            })
            .collect::<HashMap<_, _>>();
        db.get_gmod_infer_index_mut()
            .set_all_realm_file_metadata(realm_metadata);
        return;
    }

    let mut filename_hints: HashMap<FileId, Option<GmodRealm>> = HashMap::new();
    let mut dependency_hints: HashMap<FileId, HashSet<GmodRealm>> = HashMap::new();
    let mut include_edges = Vec::new();

    for file_id in &file_ids {
        let hint = if detect_filename {
            infer_realm_from_filename(db, *file_id)
        } else {
            None
        };
        filename_hints.insert(*file_id, hint);
    }

    if detect_calls {
        let dependency_index = db.get_file_dependencies_index();
        for source_file_id in &file_ids {
            let Some(dependencies) = dependency_index.get_required_files(source_file_id) else {
                continue;
            };

            for dependency_file_id in dependencies {
                match dependency_index.get_dependency_kind(source_file_id, dependency_file_id) {
                    Some(LuaDependencyKind::AddCSLuaFile) => {
                        dependency_hints
                            .entry(*source_file_id)
                            .or_default()
                            .insert(GmodRealm::Server);
                        dependency_hints
                            .entry(*dependency_file_id)
                            .or_default()
                            .insert(GmodRealm::Client);
                    }
                    Some(LuaDependencyKind::Require) => {
                        dependency_hints
                            .entry(*dependency_file_id)
                            .or_default()
                            .insert(GmodRealm::Shared);
                    }
                    Some(LuaDependencyKind::Include) => {
                        include_edges.push((*source_file_id, *dependency_file_id));
                    }
                    _ => {}
                }
            }
        }
    }

    let mut inferred_realms: HashMap<FileId, GmodRealm> = file_ids
        .iter()
        .map(|file_id| {
            (
                *file_id,
                infer_realm(
                    filename_hints.get(file_id).copied().flatten(),
                    dependency_hints.get(file_id),
                    default_realm,
                ),
            )
        })
        .collect();

    if detect_calls && !include_edges.is_empty() {
        for _ in 0..3 {
            let mut next_dependency_hints = dependency_hints.clone();
            for (source_file_id, dependency_file_id) in &include_edges {
                let source_realm = inferred_realms
                    .get(source_file_id)
                    .copied()
                    .unwrap_or(GmodRealm::Unknown);
                let dependency_realm = inferred_realms
                    .get(dependency_file_id)
                    .copied()
                    .unwrap_or(GmodRealm::Unknown);

                if source_realm != GmodRealm::Unknown {
                    next_dependency_hints
                        .entry(*dependency_file_id)
                        .or_default()
                        .insert(source_realm);
                }
                if dependency_realm != GmodRealm::Unknown {
                    next_dependency_hints
                        .entry(*source_file_id)
                        .or_default()
                        .insert(dependency_realm);
                }
            }

            let next_inferred_realms: HashMap<FileId, GmodRealm> = file_ids
                .iter()
                .map(|file_id| {
                    (
                        *file_id,
                        infer_realm(
                            filename_hints.get(file_id).copied().flatten(),
                            next_dependency_hints.get(file_id),
                            default_realm,
                        ),
                    )
                })
                .collect();

            dependency_hints = next_dependency_hints;
            if next_inferred_realms == inferred_realms {
                break;
            }

            inferred_realms = next_inferred_realms;
        }
    }

    let mut realm_metadata = HashMap::new();
    for file_id in file_ids {
        let mut hints = dependency_hints
            .remove(&file_id)
            .unwrap_or_default()
            .into_iter()
            .collect::<Vec<_>>();
        hints.sort_by_key(|realm| realm_sort_key(*realm));

        let ranges = branch_realm_ranges
            .get(&file_id)
            .cloned()
            .unwrap_or_default();

        let annotation_realm = annotation_realms.get(&file_id).copied();
        let final_realm = annotation_realm.unwrap_or_else(|| {
            inferred_realms
                .get(&file_id)
                .copied()
                .unwrap_or(default_realm)
        });

        realm_metadata.insert(
            file_id,
            GmodRealmFileMetadata {
                inferred_realm: final_realm,
                filename_hint: filename_hints.get(&file_id).copied().flatten(),
                dependency_hints: hints,
                annotation_realm,
                branch_realm_ranges: ranges,
            },
        );
    }

    db.get_gmod_infer_index_mut()
        .set_all_realm_file_metadata(realm_metadata);
}

fn infer_realm(
    filename_hint: Option<GmodRealm>,
    dependency_hints: Option<&HashSet<GmodRealm>>,
    default_realm: GmodRealm,
) -> GmodRealm {
    let mut hints = HashSet::new();
    if let Some(filename_hint) = filename_hint
        && filename_hint != GmodRealm::Unknown
    {
        hints.insert(filename_hint);
    }

    if let Some(dependency_hints) = dependency_hints {
        hints.extend(
            dependency_hints
                .iter()
                .copied()
                .filter(|realm| *realm != GmodRealm::Unknown),
        );
    }

    if hints.is_empty() {
        return default_realm;
    }

    if hints.len() == 1 {
        return *hints.iter().next().expect("len checked");
    }

    if hints.len() == 2 {
        // Shared + Client/Server → resolve to the specific realm
        if hints.contains(&GmodRealm::Shared) {
            if hints.contains(&GmodRealm::Client) {
                return GmodRealm::Client;
            }
            if hints.contains(&GmodRealm::Server) {
                return GmodRealm::Server;
            }
        }

        // Client + Server → the file runs on both realms, so it's Shared
        if hints.contains(&GmodRealm::Client) && hints.contains(&GmodRealm::Server) {
            return GmodRealm::Shared;
        }
    }

    GmodRealm::Unknown
}

fn gmod_config_default_realm(db: &DbIndex) -> GmodRealm {
    match db.get_emmyrc().gmod.default_realm {
        EmmyrcGmodRealm::Client => GmodRealm::Client,
        EmmyrcGmodRealm::Server => GmodRealm::Server,
        EmmyrcGmodRealm::Shared => GmodRealm::Shared,
        EmmyrcGmodRealm::Menu => GmodRealm::Unknown,
    }
}

fn infer_realm_from_filename(db: &DbIndex, file_id: FileId) -> Option<GmodRealm> {
    let file_path = db.get_vfs().get_file_path(&file_id)?;
    let file_name = file_path
        .file_name()?
        .to_string_lossy()
        .to_ascii_lowercase();

    if file_name == "cl_init.lua" || file_name.starts_with("cl_") {
        return Some(GmodRealm::Client);
    }

    if file_name == "init.lua" || file_name.starts_with("sv_") {
        return Some(GmodRealm::Server);
    }

    if file_name == "shared.lua" || file_name.starts_with("sh_") {
        return Some(GmodRealm::Shared);
    }

    None
}

fn realm_sort_key(realm: GmodRealm) -> u8 {
    match realm {
        GmodRealm::Client => 0,
        GmodRealm::Server => 1,
        GmodRealm::Shared => 2,
        GmodRealm::Unknown => 3,
    }
}
