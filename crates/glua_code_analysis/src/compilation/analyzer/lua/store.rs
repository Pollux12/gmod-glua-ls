//! The writes a statement's analysis makes: computed against the shared index,
//! logged, and applied by the engine afterwards.

use std::sync::Arc;

use rowan::{TextRange, TextSize};
use smol_str::SmolStr;

use crate::{
    BoundMetatable, DbIndex, FileId, GlobalId, InFiled, LuaClassNameReturnAlias, LuaDeclId,
    LuaDocReturnInfo, LuaInferenceNodeId, LuaMember, LuaMemberFeature, LuaMemberId, LuaMemberKey,
    LuaMemberOwner, LuaOperator, LuaReturnCorrelation, LuaSemanticDeclId, LuaSignatureId, LuaType,
    LuaTypeCache, LuaTypeFact, LuaTypeOwner, SignatureReturnStatus,
    compilation::analyzer::{
        common::{
            DeclWrite, TypeCacheWriteMode, add_member, bind_decl_write, bind_type, write_type_cache,
        },
        unresolve::iter_var_updates,
    },
    db_index::{
        AccessorFuncCallMetadata, GmodScriptedClassCallKind, GmodScriptedClassCallMetadata,
        SetmetatableFactoryBinding,
    },
};

use super::{
    IterVarInference, is_member_assignment_in_conditional_branch,
    member_write_policy::{
        CollectionWidening, widen_member_collections, widened_member_assignment_collection_type,
    },
    stats::{
        canonical_guarded_table_bootstrap_type, is_assignment_file_define_member,
        is_guarded_table_assignment_member,
    },
};

/// An index expression's member filed under its owner.
#[derive(Debug)]
pub(in crate::compilation::analyzer) struct MemberHoming {
    pub member_id: LuaMemberId,
    pub key: LuaMemberKey,
    pub owner: LuaMemberOwner,
    /// The member to add when the index holds none under `member_id`; `None`
    /// when the analysis found one, or found no key to add it under.
    pub new_member: Option<LuaMember>,
    pub set_owner_only: bool,
    pub guarded_table_assignment: bool,
    /// The global path the owner class is an alias of, for provenance.
    pub alias_path: Option<GlobalId>,
}

impl MemberHoming {
    fn apply(&self, db: &mut DbIndex) {
        let member_id = self.member_id;
        let file_id = member_id.file_id;
        if db.get_member_index().get_member(&member_id).is_none() {
            let Some(member) = &self.new_member else {
                return;
            };
            if self.guarded_table_assignment {
                db.get_member_index_mut()
                    .mark_non_overwriting_assignment_member(member_id);
            }
            let member_index = db.get_member_index_mut();
            member_index.add_member_with_owner_only(
                self.owner.clone(),
                member.clone(),
                self.set_owner_only,
            );
            if let Some(alias_path) = &self.alias_path {
                member_index.home_alias_member_with_provenance(
                    self.owner.clone(),
                    file_id,
                    member_id,
                    alias_path.clone(),
                    self.set_owner_only,
                );
            }
            // `add_member` records the enclosing function scope of a
            // `FileDefine` member; any other feature stores none.
            if !matches!(member.get_feature(), LuaMemberFeature::FileDefine) {
                let function_scope =
                    member_index.enclosing_function_scope_range(file_id, member_id.get_position());
                member_index.set_member_function_scope_range(member_id, function_scope);
            }
            return;
        }

        if db
            .get_member_index()
            .get_member(&member_id)
            .is_some_and(|member| member.get_key() != &self.key)
        {
            db.get_member_index_mut()
                .rekey_member(member_id, self.key.clone());
        }
        if self.guarded_table_assignment {
            db.get_member_index_mut()
                .mark_non_overwriting_assignment_member(member_id);
        }
        let function_scope = db
            .get_member_index()
            .enclosing_function_scope_range(file_id, member_id.get_position());
        if self.set_owner_only {
            let member_index = db.get_member_index_mut();
            match &self.alias_path {
                Some(alias_path) => {
                    member_index.home_alias_member_with_provenance(
                        self.owner.clone(),
                        file_id,
                        member_id,
                        alias_path.clone(),
                        true,
                    );
                }
                None => {
                    member_index.set_member_owner_only(self.owner.clone(), file_id, member_id);
                }
            }
        } else {
            add_member(db, self.owner.clone(), member_id);
        }
        db.get_member_index_mut()
            .set_member_function_scope_range(member_id, function_scope);
    }
}

/// A change to a closure's signature.
#[derive(Debug)]
pub(in crate::compilation::analyzer) enum SignatureChange {
    DirectParamReturnAlias(usize),
    /// Applies unless the signature already returns a parameter directly or
    /// by class name, or has overloads.
    ClassNameParamReturnAlias(LuaClassNameReturnAlias),
    ReturnsReceiver,
    FalsyParamReturnAlias {
        falsy_param_idx: usize,
        aliased_param_idx: usize,
        return_slot: usize,
    },
    FalsyParamNilFreeReturnSlot {
        param_idx: usize,
        return_slot: usize,
    },
    NilReturnGuardParam(usize),
    RequireGuardParam(usize),
    /// A closure without a body returns nothing it infers.
    BodilessReturn,
    InferredReturns {
        returns: Vec<LuaDocReturnInfo>,
        correlations: Vec<LuaReturnCorrelation>,
    },
}

impl SignatureChange {
    fn apply(&self, db: &mut DbIndex, signature_id: LuaSignatureId) {
        let signature = db.get_signature_index_mut().get_or_create(signature_id);
        match self {
            SignatureChange::DirectParamReturnAlias(param_idx) => {
                signature.set_direct_param_return_alias(*param_idx);
            }
            SignatureChange::ClassNameParamReturnAlias(alias) => {
                if signature.direct_param_return_alias().is_none()
                    && signature.class_name_param_return_alias().is_none()
                    && signature.overloads.is_empty()
                {
                    signature.set_class_name_param_return_alias(alias.clone());
                }
            }
            SignatureChange::ReturnsReceiver => signature.set_returns_receiver(),
            SignatureChange::FalsyParamReturnAlias {
                falsy_param_idx,
                aliased_param_idx,
                return_slot,
            } => {
                signature.add_falsy_param_return_alias(
                    *falsy_param_idx,
                    *aliased_param_idx,
                    *return_slot,
                );
            }
            SignatureChange::FalsyParamNilFreeReturnSlot {
                param_idx,
                return_slot,
            } => {
                signature.add_falsy_param_nil_free_return_slot(*param_idx, *return_slot);
            }
            SignatureChange::NilReturnGuardParam(param_idx) => {
                signature.add_nil_return_guard_param(*param_idx);
            }
            SignatureChange::RequireGuardParam(param_idx) => {
                signature.set_require_guard_param(*param_idx);
            }
            SignatureChange::BodilessReturn => {
                signature.resolve_return = SignatureReturnStatus::InferResolve;
                signature.set_return_correlations(Vec::new());
            }
            SignatureChange::InferredReturns {
                returns,
                correlations,
            } => {
                signature.resolve_return = SignatureReturnStatus::InferResolve;
                signature.return_docs = returns.clone();
                signature.set_return_correlations(correlations.clone());
                db.get_signature_index_mut().note_return_write(signature_id);
            }
        }
    }
}

/// One index write a statement's analysis made, applied through the same
/// write path an inline analysis calls, against the index as it stands when
/// it applies.
#[derive(Debug)]
pub(in crate::compilation::analyzer) enum StatementWrite {
    AddMember {
        owner: LuaMemberOwner,
        member: LuaMember,
    },
    BindType {
        owner: LuaTypeOwner,
        cache: LuaTypeCache,
    },
    WriteTypeCache {
        owner: LuaTypeOwner,
        cache: LuaTypeCache,
        mode: TypeCacheWriteMode,
    },
    BindDeclWrite {
        decl_id: LuaDeclId,
        cache: LuaTypeCache,
        write: DeclWrite,
    },
    /// Publishes an initializer's inference fact for its declaration when the
    /// declaration's stored type is the fact's type.
    PublishInitializerFact {
        decl_id: LuaDeclId,
        fact: LuaTypeFact,
    },
    HomeMember(MemberHoming),
    /// Binds the value an assignment gives its member, reconciled with the
    /// member's slot as it stands: a guarded bootstrap takes the slot's
    /// canonical table, and a collection joins its siblings' collections.
    /// Then marks the member as not overwriting its slot when the file defines
    /// it and it is guarded or written in a conditional branch.
    AssignMember {
        member_id: LuaMemberId,
        value: LuaType,
        /// Whether the member's key is an expression: its value is one of the
        /// table's elements, which its siblings' collections do not widen.
        dynamic_key: bool,
        preserve_table_literals: bool,
    },
    AddInferredStringDefault {
        file_id: FileId,
        decl_id: LuaDeclId,
        value: SmolStr,
        range: TextRange,
    },
    WidenCollections {
        member_ids: Vec<LuaMemberId>,
        incoming: CollectionWidening,
    },
    /// Widens a `for ... in` statement's variables with what their iterator
    /// binds, against the types they hold.
    IterVarUpdates {
        file_id: FileId,
        iter_var_types: IterVarInference,
        var_positions: Vec<TextSize>,
    },
    AddMetatable {
        table: InFiled<TextRange>,
        metatable: BoundMetatable,
        file_id: FileId,
        sort_key: Arc<str>,
    },
    AddFactoryBinding(SetmetatableFactoryBinding),
    AddOperator(LuaOperator),
    SetModuleExport {
        file_id: FileId,
        export_type: LuaType,
        semantic_id: Option<LuaSemanticDeclId>,
    },
    AddAccessorFuncCall {
        file_id: FileId,
        metadata: AccessorFuncCallMetadata,
    },
    AddGmodClassCall {
        file_id: FileId,
        kind: GmodScriptedClassCallKind,
        metadata: GmodScriptedClassCallMetadata,
    },
    Signature {
        signature_id: LuaSignatureId,
        change: SignatureChange,
    },
}

impl StatementWrite {
    pub(in crate::compilation::analyzer) fn apply(&self, db: &mut DbIndex) {
        match self {
            StatementWrite::AddMember { owner, member } => {
                db.get_member_index_mut()
                    .add_member(owner.clone(), member.clone());
            }
            StatementWrite::BindType { owner, cache } => {
                bind_type(db, owner.clone(), cache.clone());
            }
            StatementWrite::WriteTypeCache { owner, cache, mode } => {
                write_type_cache(db, owner.clone(), cache.clone(), *mode);
            }
            StatementWrite::BindDeclWrite {
                decl_id,
                cache,
                write,
            } => {
                bind_decl_write(db, *decl_id, cache.clone(), *write);
            }
            StatementWrite::PublishInitializerFact { decl_id, fact } => {
                let owner = LuaTypeOwner::Decl(*decl_id);
                if db
                    .get_type_index()
                    .get_type_cache(&owner)
                    .is_some_and(|cache| !cache.is_doc() && cache.as_type() == fact.typ())
                {
                    db.publish_inference_facts(vec![(
                        LuaInferenceNodeId::TypeOwner(owner),
                        fact.clone(),
                    )]);
                }
            }
            StatementWrite::HomeMember(homing) => homing.apply(db),
            StatementWrite::AssignMember {
                member_id,
                value,
                dynamic_key,
                preserve_table_literals,
            } => {
                let member_id = *member_id;
                let canonical = if is_guarded_table_assignment_member(db, member_id) {
                    canonical_guarded_table_bootstrap_type(db, member_id)
                } else {
                    None
                };
                let value = match canonical {
                    Some(canonical) => canonical,
                    None if !dynamic_key => {
                        widened_member_assignment_collection_type(db, member_id, value)
                            .unwrap_or_else(|| value.clone())
                    }
                    None => value.clone(),
                };
                bind_type(db, member_id.into(), LuaTypeCache::InferType(value));
                if is_assignment_file_define_member(db, member_id)
                    && (*preserve_table_literals
                        || is_member_assignment_in_conditional_branch(db, member_id))
                {
                    db.get_member_index_mut()
                        .mark_non_overwriting_assignment_member(member_id);
                }
            }
            StatementWrite::AddInferredStringDefault {
                file_id,
                decl_id,
                value,
                range,
            } => {
                db.get_property_index_mut().add_inferred_string_default(
                    *file_id,
                    *decl_id,
                    value.clone(),
                    *range,
                );
            }
            StatementWrite::WidenCollections {
                member_ids,
                incoming,
            } => {
                widen_member_collections(db, member_ids, incoming);
            }
            StatementWrite::IterVarUpdates {
                file_id,
                iter_var_types,
                var_positions,
            } => {
                for update in iter_var_updates(db, *file_id, iter_var_types, var_positions) {
                    write_type_cache(db, update.owner, update.cache, update.mode);
                }
            }
            StatementWrite::AddMetatable {
                table,
                metatable,
                file_id,
                sort_key,
            } => {
                let index = db.get_metatable_index_mut();
                match metatable {
                    BoundMetatable::Table(metatable) => {
                        index.add(table.clone(), metatable.clone(), *file_id, sort_key.clone())
                    }
                    BoundMetatable::Opaque(expr) => {
                        index.add_opaque(table.clone(), expr.clone(), *file_id, sort_key.clone())
                    }
                }
            }
            StatementWrite::AddFactoryBinding(binding) => {
                db.get_metatable_index_mut()
                    .add_factory_binding(binding.clone());
            }
            StatementWrite::AddOperator(operator) => {
                db.add_operator(operator.clone());
            }
            StatementWrite::SetModuleExport {
                file_id,
                export_type,
                semantic_id,
            } => {
                db.get_module_index_mut().set_export(
                    *file_id,
                    export_type.clone(),
                    semantic_id.clone(),
                );
            }
            StatementWrite::AddAccessorFuncCall { file_id, metadata } => {
                db.get_accessor_func_call_index_mut()
                    .add_call(*file_id, metadata.clone());
            }
            StatementWrite::AddGmodClassCall {
                file_id,
                kind,
                metadata,
            } => {
                db.get_gmod_class_metadata_index_mut()
                    .add_call(*file_id, *kind, metadata.clone());
            }
            StatementWrite::Signature {
                signature_id,
                change,
            } => change.apply(db, *signature_id),
        }
    }
}
