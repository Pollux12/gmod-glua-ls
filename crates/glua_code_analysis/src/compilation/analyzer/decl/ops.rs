use std::sync::Arc;

use glua_parser::{LuaSyntaxId, LuaVersionCondition};
use rowan::{TextRange, TextSize};
use rustc_hash::FxHashMap;
use smol_str::SmolStr;

use crate::{
    FileId, InFiled, LuaMemberId, LuaSemanticDeclId, LuaSignatureId, LuaTypeOwner, WorkspaceId,
    compilation::analyzer::{
        AnalyzeContext,
        common::{TypeCacheWriteMode, bind_type, write_type_cache},
        gmod::{ensure_scoped_class_type_decl_for_file, resolve_load_dependency_target},
    },
    db_index::{
        DbIndex, FileReferenceBatch, GlobalId, GmodScopedClassInfo, LegacyModuleEnv, LuaDecl,
        LuaDeclId, LuaDeclarationTree, LuaDependencyKind, LuaDependencySite, LuaMember,
        LuaMemberOwner, LuaTypeCache, LuaTypeDecl,
    },
};

/// A write the declaration walk makes to the index, kept for
/// [`DeclWalk::apply`] to make in the order the walk found it.
pub(super) enum DeclOp {
    FunctionScopeRange(TextRange),
    DefinitionSite {
        path: GlobalId,
        site: InFiled<TextRange>,
    },
    AddMember {
        owner: LuaMemberOwner,
        member: LuaMember,
    },
    GlobalDecl {
        name: String,
        decl_id: LuaDeclId,
        file_order_key: Arc<str>,
    },
    OwnerMap {
        source: LuaSemanticDeclId,
        same: LuaSemanticDeclId,
    },
    /// What a closure's parameter list gives its signature; `is_vararg` is set
    /// only by a list that was read to its end.
    Signature {
        id: LuaSignatureId,
        is_colon_define: bool,
        params: Vec<SmolStr>,
        is_vararg: Option<bool>,
    },
    BindLocalFuncDecl {
        signature_id: LuaSignatureId,
        decl_id: LuaDeclId,
    },
    BindType {
        owner: LuaTypeOwner,
        cache: LuaTypeCache,
    },
    WriteTypeCache {
        owner: LuaTypeOwner,
        cache: LuaTypeCache,
    },
    FileNamespace(String),
    FileUsingNamespace(String),
    TypeDecl(LuaTypeDecl),
    ModuleMeta,
    ModuleHidden,
    ModuleName {
        name: String,
        workspace_id: WorkspaceId,
    },
    ModuleVersions(Vec<LuaVersionCondition>),
    LegacyModuleEnv(LegacyModuleEnv),
    /// A load call. Its target is looked up when the op is applied: it depends
    /// on the modules the files before it registered.
    Dependency {
        kind: LuaDependencyKind,
        path: Option<String>,
        self_reference: bool,
        path_keys: Vec<String>,
        original_expr: String,
        call_range: TextRange,
        /// The path argument's range, or the call's when it has no argument.
        range: TextRange,
    },
}

/// What the walk adds to the file's [`AnalyzeContext`].
#[derive(Default)]
pub(super) struct ContextDelta {
    pub(super) meta: bool,
    pub(super) early_callable_signatures: Vec<(LuaTypeOwner, LuaSignatureId)>,
    pub(super) early_member_owner_candidates: Vec<(LuaMemberId, LuaDeclId)>,
    pub(super) inferred_guard_candidates: Vec<InFiled<LuaSyntaxId>>,
}

/// What walking one file's declarations found.
pub(in crate::compilation::analyzer) struct DeclWalk {
    pub(super) file_id: FileId,
    pub(super) scoped_class_info: Option<GmodScopedClassInfo>,
    pub(super) chunk_range: TextRange,
    pub(super) estimate: usize,
    pub(super) references: FileReferenceBatch,
    pub(super) ops: Vec<DeclOp>,
    pub(super) decl_tree: LuaDeclarationTree,
    pub(super) reassignments: FxHashMap<LuaDeclId, TextSize>,
    pub(super) delta: ContextDelta,
}

impl DeclWalk {
    /// Writes the walk to the index, as the declaration walk made directly
    /// would, and adds what it found to `context`, the context of the file's
    /// source layer. Runs inside the file's source evaluation.
    pub(in crate::compilation::analyzer) fn apply(
        self,
        db: &mut DbIndex,
        context: &mut AnalyzeContext,
    ) {
        let file_id = self.file_id;
        if let Some(info) = &self.scoped_class_info {
            db.get_gmod_infer_index_mut()
                .set_scoped_class_info(file_id, info.clone());
        }
        db.get_reference_index_mut().create_local_reference(file_id);
        db.get_member_index_mut().add_file_entries(file_id);
        let references = db.get_reference_index_mut();
        references.reserve_for_file(file_id, self.estimate);
        references.file_batch(self.references);
        let table_literal_decls = self
            .decl_tree
            .get_decls()
            .values()
            .filter(|decl| decl.table_literal_initializer().is_some())
            .map(LuaDecl::get_id)
            .collect();
        db.get_type_index_mut()
            .set_table_literal_decls(file_id, table_literal_decls);
        for op in self.ops {
            op.apply(db, file_id);
        }
        if self.scoped_class_info.is_some() {
            ensure_scoped_class_type_decl_for_file(db, file_id, self.chunk_range);
        }
        db.get_decl_index_mut().add_decl_tree(self.decl_tree);
        db.get_reference_index_mut()
            .set_local_reassignments(file_id, self.reassignments);

        let delta = self.delta;
        if delta.meta {
            context.add_meta(file_id);
        }
        for (type_owner, signature_id) in delta.early_callable_signatures {
            context.add_early_callable_signature(type_owner, signature_id);
        }
        for (member_id, decl_id) in delta.early_member_owner_candidates {
            context.add_early_member_owner_candidate(member_id, decl_id);
        }
        for candidate in delta.inferred_guard_candidates {
            context.add_inferred_guard_candidate(candidate);
        }
    }
}

impl DeclOp {
    fn apply(self, db: &mut DbIndex, file_id: FileId) {
        match self {
            Self::FunctionScopeRange(range) => db
                .get_member_index_mut()
                .add_function_scope_range(file_id, range),
            Self::DefinitionSite { path, site } => {
                db.get_member_index_mut().set_definition_site(path, site)
            }
            Self::AddMember { owner, member } => {
                db.get_member_index_mut().add_member(owner, member);
            }
            Self::GlobalDecl {
                name,
                decl_id,
                file_order_key,
            } => db
                .get_global_index_mut()
                .add_global_decl(&name, decl_id, file_order_key),
            Self::OwnerMap { source, same } => {
                db.get_property_index_mut()
                    .add_owner_map(source, same, file_id);
            }
            Self::Signature {
                id,
                is_colon_define,
                params,
                is_vararg,
            } => {
                let signature = db.get_signature_index_mut().get_or_create(id);
                signature.is_colon_define = is_colon_define;
                signature.params.extend(params);
                if let Some(is_vararg) = is_vararg {
                    signature.is_vararg = is_vararg;
                }
            }
            Self::BindLocalFuncDecl {
                signature_id,
                decl_id,
            } => db
                .get_signature_index_mut()
                .bind_local_func_decl(signature_id, decl_id),
            Self::BindType { owner, cache } => {
                bind_type(db, owner, cache);
            }
            Self::WriteTypeCache { owner, cache } => {
                write_type_cache(db, owner, cache, TypeCacheWriteMode::InsertOnly)
            }
            Self::FileNamespace(namespace) => db
                .get_type_index_mut()
                .add_file_namespace(file_id, namespace),
            Self::FileUsingNamespace(namespace) => db
                .get_type_index_mut()
                .add_file_using_namespace(file_id, namespace),
            Self::TypeDecl(decl) => db.get_type_index_mut().add_type_decl(file_id, decl),
            Self::ModuleMeta => db.get_module_index_mut().set_meta(file_id),
            Self::ModuleHidden => db
                .get_module_index_mut()
                .set_module_visibility(file_id, false),
            Self::ModuleName { name, workspace_id } => {
                let modules = db.get_module_index_mut();
                modules.add_module_by_module_path(file_id, name, workspace_id);
                modules.set_meta(file_id);
            }
            Self::ModuleVersions(conditions) => db
                .get_module_index_mut()
                .set_module_version_conds(file_id, conditions),
            Self::LegacyModuleEnv(env) => db
                .get_module_index_mut()
                .set_legacy_module_env(file_id, env),
            Self::Dependency {
                kind,
                path,
                self_reference,
                path_keys,
                original_expr,
                call_range,
                range,
            } => {
                let target_file_id = if let Some(path) = &path {
                    resolve_load_dependency_target(db, file_id, kind, path)
                } else if self_reference {
                    Some(file_id)
                } else {
                    None
                };
                db.get_file_dependencies_index_mut()
                    .add_dependency_site(LuaDependencySite {
                        source_file_id: file_id,
                        target_file_id,
                        kind,
                        path,
                        path_keys,
                        original_expr,
                        call_range,
                        range,
                    });
            }
        }
    }
}
