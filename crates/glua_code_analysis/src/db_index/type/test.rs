#[cfg(test)]
mod test {
    use std::{cmp::Ordering, collections::HashSet, sync::Arc};

    use glua_parser::{LuaKind, LuaSyntaxId, LuaSyntaxKind};
    use rowan::TextRange;

    use crate::db_index::traits::LuaIndex;
    use crate::db_index::r#type::LuaTypeIndex;
    use crate::db_index::{LuaDeclTypeKind, LuaTypeFlag};
    use crate::{
        DbIndex, FileId, InFiled, LuaDeclId, LuaDeclLocation, LuaDefinitionId,
        LuaInferenceConfidence, LuaInferenceEventId, LuaInferenceNodeId,
        LuaInferenceProvenanceKind, LuaInferenceStep, LuaType, LuaTypeCache, LuaTypeDecl,
        LuaTypeDeclId, LuaTypeFact, LuaTypeFactMetadata, LuaTypeOwner, resolve_alias_type,
    };

    fn create_type_index() -> LuaTypeIndex {
        LuaTypeIndex::new()
    }

    fn file_id() -> FileId {
        FileId::new(1)
    }

    fn owner() -> LuaTypeOwner {
        LuaTypeOwner::Decl(LuaDeclId::new(file_id(), 10.into()))
    }

    fn owner_in(file_id: FileId, position: u32) -> LuaTypeOwner {
        LuaTypeOwner::Decl(LuaDeclId::new(file_id, position.into()))
    }

    fn source(position: u32) -> InFiled<LuaSyntaxId> {
        source_in(file_id(), position)
    }

    fn source_in(file_id: FileId, position: u32) -> InFiled<LuaSyntaxId> {
        InFiled::new(
            file_id,
            LuaSyntaxId::new(
                LuaKind::Syntax(LuaSyntaxKind::LocalName),
                TextRange::new(position.into(), (position + 1).into()),
            ),
        )
    }

    fn anchored_metadata() -> LuaTypeFactMetadata {
        let event = LuaInferenceEventId {
            node: LuaInferenceNodeId::TypeOwner(owner()),
            kind: LuaInferenceProvenanceKind::ContextualUnknown,
            source: source(20),
        };
        LuaTypeFactMetadata {
            confidence: LuaInferenceConfidence::Anchored,
            base_provenance_kind: None,
            provenance: Arc::from([LuaInferenceStep {
                event,
                support: Arc::from([]),
                found_type: None,
            }]),
        }
    }

    #[test]
    fn inference_fact_runtime_type_change_preserves_epistemic_metadata() {
        let fact = LuaTypeFact::new(
            LuaType::from_vec(vec![LuaType::Nil, LuaType::String]),
            anchored_metadata().confidence,
            anchored_metadata().provenance,
        );

        let narrowed = fact.with_runtime_type(LuaType::String);

        assert_eq!(narrowed.typ(), &LuaType::String);
        assert_eq!(narrowed.confidence(), LuaInferenceConfidence::Anchored);
        assert_eq!(narrowed.base_provenance_kind(), fact.base_provenance_kind());
        assert_eq!(narrowed.provenance(), fact.provenance());
    }

    #[test]
    fn inference_fact_stable_order_and_deduplication_are_deterministic() {
        let first = LuaInferenceEventId {
            node: LuaInferenceNodeId::TypeOwner(owner()),
            kind: LuaInferenceProvenanceKind::ContextualUnknown,
            source: source(20),
        };
        let second = LuaInferenceEventId {
            node: LuaInferenceNodeId::TypeOwner(owner()),
            kind: LuaInferenceProvenanceKind::UnguardedChild,
            source: source(30),
        };
        let fact = LuaTypeFact::new(
            LuaType::String,
            LuaInferenceConfidence::Anchored,
            Arc::from([
                LuaInferenceStep {
                    event: second.clone(),
                    found_type: None,
                    support: Arc::from([]),
                },
                LuaInferenceStep {
                    event: first.clone(),
                    found_type: None,
                    support: Arc::from([]),
                },
                LuaInferenceStep {
                    event: second.clone(),
                    found_type: None,
                    support: Arc::from([]),
                },
            ]),
        );

        assert_eq!(
            fact.diagnostic_events().collect::<Vec<_>>(),
            vec![&second, &first]
        );
        assert_eq!(first.stable_cmp(&second), Ordering::Less);
        assert_eq!(second.stable_cmp(&first), Ordering::Greater);
    }

    #[test]
    fn inference_fact_index_force_plain_replacement_clears_uncertain_metadata() {
        let mut index = LuaTypeIndex::new();
        index.force_bind_type_fact(
            owner(),
            LuaTypeCache::InferType(LuaType::String),
            anchored_metadata(),
        );
        index.force_bind_type(owner(), LuaTypeCache::InferType(LuaType::Number));

        let fact = index.get_type_fact(&owner()).unwrap();
        assert_eq!(fact.typ(), &LuaType::Number);
        assert_eq!(fact.confidence(), LuaInferenceConfidence::Certain);
        assert!(fact.provenance().is_empty());
        assert!(index.get_inference_events_for_file(file_id()).is_empty());
    }

    #[test]
    fn inference_fact_index_insert_only_preserves_existing_fact() {
        let mut index = LuaTypeIndex::new();
        index.bind_type_fact(
            owner(),
            LuaTypeCache::InferType(LuaType::String),
            anchored_metadata(),
        );
        index.bind_type(owner(), LuaTypeCache::InferType(LuaType::Number));

        let fact = index.get_type_fact(&owner()).unwrap();
        assert_eq!(fact.typ(), &LuaType::String);
        assert_eq!(fact.confidence(), LuaInferenceConfidence::Anchored);
    }

    #[test]
    fn inference_fact_plain_caches_expose_base_provenance_without_synthetic_events() {
        let mut index = LuaTypeIndex::new();
        let doc_owner = owner_in(file_id(), 10);
        let infer_owner = owner_in(file_id(), 20);
        index.bind_type(doc_owner.clone(), LuaTypeCache::DocType(LuaType::String));
        index.bind_type(
            infer_owner.clone(),
            LuaTypeCache::InferType(LuaType::Number),
        );

        let doc_fact = index.get_type_fact(&doc_owner).unwrap();
        let infer_fact = index.get_type_fact(&infer_owner).unwrap();

        assert_eq!(doc_fact.confidence(), LuaInferenceConfidence::Certain);
        assert_eq!(
            doc_fact.base_provenance_kind(),
            Some(LuaInferenceProvenanceKind::ExplicitAnnotation)
        );
        assert!(doc_fact.provenance().is_empty());
        assert_eq!(infer_fact.confidence(), LuaInferenceConfidence::Certain);
        assert_eq!(
            infer_fact.base_provenance_kind(),
            Some(LuaInferenceProvenanceKind::ConcreteValue)
        );
        assert!(infer_fact.provenance().is_empty());
    }

    #[test]
    fn inference_fact_authority_order_rejects_self_confirming_upgrades() {
        let target = LuaInferenceNodeId::TypeOwner(owner());
        let contextual = LuaTypeFact::new(
            LuaType::Number,
            LuaInferenceConfidence::Anchored,
            anchored_metadata().provenance,
        );
        let concrete = LuaTypeFact::certain(LuaType::Number);
        let explicit = LuaTypeFact::from_normalized_parts(
            LuaType::Number,
            LuaInferenceConfidence::Certain,
            Some(LuaInferenceProvenanceKind::ExplicitAnnotation),
            Arc::from([]),
        );
        let cyclic = LuaTypeFact::new(
            LuaType::Number,
            LuaInferenceConfidence::Certain,
            Arc::from([LuaInferenceStep {
                event: LuaInferenceEventId {
                    node: target.clone(),
                    kind: LuaInferenceProvenanceKind::ConcreteValue,
                    source: source(30),
                },
                support: Arc::from([target.clone()]),
                found_type: None,
            }]),
        );

        assert!(concrete.has_independently_stronger_authority_than(&contextual, &target));
        assert!(explicit.has_independently_stronger_authority_than(&concrete, &target));
        assert!(!contextual.has_independently_stronger_authority_than(&concrete, &target));
        assert!(!cyclic.has_independently_stronger_authority_than(&contextual, &target));
    }

    #[test]
    fn declared_base_does_not_replace_its_unguarded_child_runtime_refinement() {
        let target = LuaInferenceNodeId::TypeOwner(owner());
        let base_type = LuaType::Ref(LuaTypeDeclId::global("Entity"));
        let refined_type = LuaType::Ref(LuaTypeDeclId::global("Player"));
        let unguarded_child = LuaTypeFact::new(
            refined_type,
            LuaInferenceConfidence::Heuristic,
            Arc::from([LuaInferenceStep {
                event: LuaInferenceEventId {
                    node: LuaInferenceNodeId::Definition(LuaDefinitionId::Declaration(
                        LuaDeclId::new(file_id(), 10.into()),
                    )),
                    kind: LuaInferenceProvenanceKind::UnguardedChild,
                    source: source(30),
                },
                support: Arc::from([]),
                found_type: Some(Arc::new(base_type.clone())),
            }]),
        );
        let declared_base = LuaTypeFact::from_normalized_parts(
            base_type.clone(),
            LuaInferenceConfidence::Certain,
            Some(LuaInferenceProvenanceKind::ExplicitAnnotation),
            Arc::from([]),
        );
        let changed_declaration = LuaTypeFact::from_normalized_parts(
            LuaType::Number,
            LuaInferenceConfidence::Certain,
            Some(LuaInferenceProvenanceKind::ExplicitAnnotation),
            Arc::from([]),
        );

        assert!(
            !declared_base.has_independently_stronger_authority_than(&unguarded_child, &target)
        );
        assert!(
            changed_declaration
                .has_independently_stronger_authority_than(&unguarded_child, &target)
        );

        let inherited_receiver_provenance = LuaTypeFact::new(
            LuaType::Ref(LuaTypeDeclId::global("Vector")),
            LuaInferenceConfidence::Heuristic,
            Arc::from([LuaInferenceStep {
                event: LuaInferenceEventId {
                    node: LuaInferenceNodeId::Definition(LuaDefinitionId::Declaration(
                        LuaDeclId::new(file_id(), 20.into()),
                    )),
                    kind: LuaInferenceProvenanceKind::UnguardedChild,
                    source: source(30),
                },
                support: Arc::from([]),
                found_type: Some(Arc::new(base_type)),
            }]),
        );
        assert!(
            declared_base.has_independently_stronger_authority_than(
                &inherited_receiver_provenance,
                &target,
            )
        );
    }

    #[test]
    fn ref_type_cache_tracks_the_declaring_file_as_an_incremental_dependency() {
        let provider = FileId::new(1);
        let consumer = FileId::new(2);
        let type_id = LuaTypeDeclId::global("ProviderType");
        let mut index = LuaTypeIndex::new();
        index.add_type_decl(
            provider,
            LuaTypeDecl::new(
                provider,
                TextRange::new(0.into(), 1.into()),
                "ProviderType".to_string(),
                LuaDeclTypeKind::Class,
                LuaTypeFlag::None.into(),
                type_id.clone(),
            ),
        );
        index.bind_type(
            owner_in(consumer, 10),
            LuaTypeCache::DocType(LuaType::Ref(type_id)),
        );

        assert_eq!(
            index.files_with_type_caches_referencing_files(&HashSet::from([provider])),
            HashSet::from([consumer])
        );
    }

    #[test]
    fn ref_type_dependency_excludes_files_that_contribute_to_the_same_type() {
        let provider = FileId::new(1);
        let contributor = FileId::new(2);
        let consumer = FileId::new(3);
        let type_id = LuaTypeDeclId::global("SharedType");
        let mut index = LuaTypeIndex::new();
        index.add_type_decl(
            provider,
            LuaTypeDecl::new(
                provider,
                TextRange::new(0.into(), 1.into()),
                "SharedType".to_string(),
                LuaDeclTypeKind::Class,
                LuaTypeFlag::None.into(),
                type_id.clone(),
            ),
        );
        index.add_type_decl_location(
            contributor,
            &type_id,
            LuaDeclLocation {
                file_id: contributor,
                range: TextRange::new(0.into(), 1.into()),
                flag: LuaTypeFlag::None.into(),
            },
        );
        index.bind_type(
            owner_in(contributor, 10),
            LuaTypeCache::DocType(LuaType::Ref(type_id.clone())),
        );
        index.bind_type(
            owner_in(consumer, 10),
            LuaTypeCache::DocType(LuaType::Ref(type_id)),
        );

        assert_eq!(
            index
                .files_with_cross_file_type_caches_referencing_files(&HashSet::from([contributor])),
            HashSet::from([consumer])
        );
    }

    #[test]
    fn inference_fact_plain_inferred_any_is_uncertain_but_nil_and_never_are_runtime_facts() {
        let mut index = LuaTypeIndex::new();
        let any_owner = owner_in(file_id(), 10);
        let nil_owner = owner_in(file_id(), 20);
        let never_owner = owner_in(file_id(), 30);
        let unknown_owner = owner_in(file_id(), 40);
        index.bind_type(any_owner.clone(), LuaTypeCache::InferType(LuaType::Any));
        index.bind_type(nil_owner.clone(), LuaTypeCache::InferType(LuaType::Nil));
        index.bind_type(never_owner.clone(), LuaTypeCache::InferType(LuaType::Never));
        index.bind_type(
            unknown_owner.clone(),
            LuaTypeCache::InferType(LuaType::Unknown),
        );

        let any_fact = index.get_type_fact(&any_owner).unwrap();
        let nil_fact = index.get_type_fact(&nil_owner).unwrap();
        let never_fact = index.get_type_fact(&never_owner).unwrap();
        let unknown_fact = index.get_type_fact(&unknown_owner).unwrap();

        assert_eq!(any_fact.typ(), &LuaType::Any);
        assert_eq!(any_fact.confidence(), LuaInferenceConfidence::Unknown);
        assert_eq!(any_fact.base_provenance_kind(), None);
        assert_eq!(nil_fact.typ(), &LuaType::Nil);
        assert_eq!(nil_fact.confidence(), LuaInferenceConfidence::Certain);
        assert_eq!(
            nil_fact.base_provenance_kind(),
            Some(LuaInferenceProvenanceKind::ConcreteValue)
        );
        assert_eq!(never_fact.typ(), &LuaType::Never);
        assert_eq!(never_fact.confidence(), LuaInferenceConfidence::Certain);
        assert_eq!(
            never_fact.base_provenance_kind(),
            Some(LuaInferenceProvenanceKind::ConcreteValue)
        );
        assert_eq!(unknown_fact, LuaTypeFact::unknown());
    }

    #[test]
    fn inference_fact_cross_file_events_are_buckets_by_source_and_invalidated_by_owner() {
        let mut index = LuaTypeIndex::new();
        let owner_file = FileId::new(1);
        let source_file = FileId::new(2);
        let owner = owner_in(owner_file, 10);
        let event = LuaInferenceEventId {
            node: LuaInferenceNodeId::TypeOwner(owner.clone()),
            kind: LuaInferenceProvenanceKind::ContextualUnknown,
            source: source_in(source_file, 20),
        };
        let metadata = LuaTypeFactMetadata {
            confidence: LuaInferenceConfidence::Anchored,
            base_provenance_kind: None,
            provenance: Arc::from([LuaInferenceStep {
                event,
                support: Arc::from([]),
                found_type: None,
            }]),
        };

        index.force_bind_type_fact(
            owner.clone(),
            LuaTypeCache::InferType(LuaType::String),
            metadata.clone(),
        );
        assert!(index.get_inference_events_for_file(owner_file).is_empty());
        assert_eq!(index.get_inference_events_for_file(source_file).len(), 1);

        index.force_bind_type(owner.clone(), LuaTypeCache::InferType(LuaType::Number));
        assert!(index.get_inference_events_for_file(source_file).is_empty());

        index.force_bind_type_fact(owner, LuaTypeCache::InferType(LuaType::String), metadata);
        assert_eq!(index.get_inference_events_for_file(source_file).len(), 1);
        index.remove(owner_file);
        assert!(index.get_inference_events_for_file(source_file).is_empty());
    }

    #[test]
    fn inference_fact_cross_file_rebuild_preserves_other_owner_events_in_same_source_bucket() {
        let source_file = FileId::new(3);
        let first_owner = owner_in(FileId::new(1), 10);
        let second_owner = owner_in(FileId::new(2), 20);
        let metadata_for = |owner: LuaTypeOwner, position| LuaTypeFactMetadata {
            confidence: LuaInferenceConfidence::Anchored,
            base_provenance_kind: None,
            provenance: Arc::from([LuaInferenceStep {
                event: LuaInferenceEventId {
                    node: LuaInferenceNodeId::TypeOwner(owner),
                    kind: LuaInferenceProvenanceKind::ContextualUnknown,
                    source: source_in(source_file, position),
                },
                found_type: None,
                support: Arc::from([]),
            }]),
        };
        let mut index = LuaTypeIndex::new();
        index.force_bind_type_fact(
            first_owner.clone(),
            LuaTypeCache::InferType(LuaType::String),
            metadata_for(first_owner.clone(), 30),
        );
        index.force_bind_type_fact(
            second_owner.clone(),
            LuaTypeCache::InferType(LuaType::Number),
            metadata_for(second_owner.clone(), 40),
        );

        index.force_bind_type(first_owner, LuaTypeCache::InferType(LuaType::Boolean));

        let remaining = index.get_inference_events_for_file(source_file);
        assert_eq!(remaining.len(), 1);
        assert_eq!(
            remaining[0].event.node,
            LuaInferenceNodeId::TypeOwner(second_owner)
        );
    }

    #[test]
    fn inference_fact_conflicting_duplicate_publications_are_rejected_independently_of_order() {
        let node = LuaInferenceNodeId::TypeOwner(owner());
        let anchored = LuaTypeFact::new(
            LuaType::String,
            LuaInferenceConfidence::Anchored,
            anchored_metadata().provenance,
        );
        let heuristic = LuaTypeFact::new(
            LuaType::Number,
            LuaInferenceConfidence::Heuristic,
            Arc::from([]),
        );

        let mut forward = DbIndex::new();
        let mut reverse = DbIndex::new();
        assert!(
            forward
                .publish_inference_facts(vec![
                    (node.clone(), anchored.clone()),
                    (node.clone(), heuristic.clone())
                ])
                .is_empty()
        );
        assert!(
            reverse
                .publish_inference_facts(vec![(node.clone(), heuristic), (node.clone(), anchored)])
                .is_empty()
        );
        assert_eq!(forward.get_inference_fact(&node), None);
        assert_eq!(reverse.get_inference_fact(&node), None);
    }

    #[test]
    fn inference_fact_definition_storage_and_reverse_support_are_file_scoped() {
        let mut index = LuaTypeIndex::new();
        let definition = LuaDefinitionId::Declaration(LuaDeclId::new(file_id(), 10.into()));
        let support_file = FileId::new(2);
        let fact = LuaTypeFact::new(
            LuaType::String,
            LuaInferenceConfidence::Anchored,
            Arc::from([LuaInferenceStep {
                event: LuaInferenceEventId {
                    node: LuaInferenceNodeId::Definition(definition),
                    kind: LuaInferenceProvenanceKind::ContextualUnknown,
                    source: InFiled::new(
                        support_file,
                        LuaSyntaxId::new(
                            LuaKind::Syntax(LuaSyntaxKind::NameExpr),
                            TextRange::new(5.into(), 6.into()),
                        ),
                    ),
                },
                found_type: None,
                support: Arc::from([LuaInferenceNodeId::TypeOwner(LuaTypeOwner::SyntaxId(
                    InFiled::new(
                        support_file,
                        LuaSyntaxId::new(
                            LuaKind::Syntax(LuaSyntaxKind::NameExpr),
                            TextRange::new(5.into(), 6.into()),
                        ),
                    ),
                ))]),
            }]),
        );

        index.bind_definition_fact(definition, fact.clone());

        assert_eq!(index.get_definition_fact(&definition), Some(&fact));
        assert_eq!(
            index.files_depending_on_inference_support(&HashSet::from([support_file])),
            HashSet::from([file_id()])
        );
    }

    #[test]
    fn inference_fact_file_removal_clears_all_sparse_fact_state() {
        let mut index = LuaTypeIndex::new();
        let definition = LuaDefinitionId::Declaration(LuaDeclId::new(file_id(), 10.into()));
        index.force_bind_type_fact(
            owner(),
            LuaTypeCache::InferType(LuaType::String),
            anchored_metadata(),
        );
        index.bind_definition_fact(definition, LuaTypeFact::certain(LuaType::Number));

        index.remove(file_id());

        assert!(index.get_type_fact(&owner()).is_none());
        assert!(index.get_definition_fact(&definition).is_none());
        assert!(index.get_inference_events_for_file(file_id()).is_empty());
    }

    #[test]
    fn inference_fact_table_const_replacement_keeps_event_fact_in_sync() {
        let mut index = LuaTypeIndex::new();
        let table_range = InFiled::new(file_id(), TextRange::new(30.into(), 32.into()));
        index.force_bind_type_fact(
            owner(),
            LuaTypeCache::InferType(LuaType::TableConst(table_range.clone())),
            anchored_metadata(),
        );

        index.replace_table_const_type(&table_range, &LuaType::Table);

        assert_eq!(
            index.get_type_fact(&owner()).unwrap().typ(),
            &LuaType::Table
        );
        assert_eq!(
            index.get_inference_events_for_file(file_id())[0].fact.typ(),
            &LuaType::Table
        );
    }

    #[test]
    fn inference_fact_db_publish_uses_the_canonical_owner_fact() {
        let mut db = DbIndex::new();
        let fact = LuaTypeFact::new(
            LuaType::String,
            LuaInferenceConfidence::Anchored,
            anchored_metadata().provenance,
        );
        let node = LuaInferenceNodeId::TypeOwner(owner());

        assert_eq!(
            db.publish_inference_facts(vec![(node.clone(), fact.clone())]),
            HashSet::from([file_id()])
        );
        assert_eq!(db.get_inference_fact(&node), Some(fact));
    }

    #[test]
    fn test_resolve_alias_type_handles_def_alias() {
        let mut db = DbIndex::default();
        let file_id = FileId { id: 1 };
        let origin_id = LuaTypeDeclId::global("Test2");
        let alias_id = LuaTypeDeclId::global("TestAlias");

        db.get_type_index_mut().add_type_decl(
            file_id,
            LuaTypeDecl::new(
                file_id,
                TextRange::new(0.into(), 0.into()),
                "Test2".to_string(),
                LuaDeclTypeKind::Class,
                LuaTypeFlag::None.into(),
                origin_id.clone(),
            ),
        );

        let mut alias = LuaTypeDecl::new(
            file_id,
            TextRange::new(0.into(), 0.into()),
            "TestAlias".to_string(),
            LuaDeclTypeKind::Alias,
            LuaTypeFlag::None.into(),
            alias_id.clone(),
        );
        alias.add_alias_origin(LuaType::Ref(origin_id.clone()));
        db.get_type_index_mut().add_type_decl(file_id, alias);

        let resolved = resolve_alias_type(&db, &LuaType::Def(alias_id.clone()));

        assert_eq!(resolved.alias_id, Some(alias_id));
        assert_eq!(resolved.typ, LuaType::Ref(origin_id));
    }

    #[test]
    fn test_namespace() {
        let mut index = create_type_index();
        let file_id = FileId { id: 1 };
        index.add_file_namespace(file_id, "test".to_string());
        let ns = index.get_file_namespace(&file_id).unwrap();
        assert_eq!(ns, "test");

        let _ = index.add_type_decl(
            file_id,
            LuaTypeDecl::new(
                file_id,
                TextRange::new(0.into(), 4.into()),
                "new_type".to_string(),
                LuaDeclTypeKind::Alias,
                LuaTypeFlag::Partial.into(),
                LuaTypeDeclId::global("test.new_type"),
            ),
        );

        let decl = index.find_type_decl(file_id, "new_type");
        assert!(decl.is_some());
        assert_eq!(decl.unwrap().get_name(), "new_type");
        assert!(decl.unwrap().is_alias());
        assert_eq!(decl.unwrap().get_id().get_name(), "test.new_type");

        let file_id2 = FileId { id: 2 };
        let decl2 = index.find_type_decl(file_id2, "test.new_type");
        assert!(decl2.is_some());
        assert_eq!(decl2, decl);

        let file_id = FileId { id: 3 };
        let decl3 = index.find_type_decl(file_id, "unknown_type");
        assert!(decl3.is_none());
    }

    #[test]
    fn test_using_namespace() {
        let mut index = create_type_index();
        let file_id = FileId { id: 1 };
        index.add_file_using_namespace(file_id, "test".to_string());
        let ns = index.get_file_using_namespace(&file_id).unwrap();
        assert_eq!(ns, &["test".to_string()]);

        let _ = index.add_type_decl(
            file_id,
            LuaTypeDecl::new(
                file_id,
                TextRange::new(0.into(), 4.into()),
                "new_type".to_string(),
                LuaDeclTypeKind::Alias,
                LuaTypeFlag::Partial.into(),
                LuaTypeDeclId::global("test.new_type"),
            ),
        );

        let decl = index.find_type_decl(file_id, "new_type");
        assert!(decl.is_some());
        assert_eq!(decl.unwrap().get_name(), "new_type");
        assert!(decl.unwrap().is_alias());

        let decl2 = index.find_type_decl(file_id, "test.new_type");
        assert!(decl2.is_some());
        assert_eq!(decl2, decl);

        let decl3 = index.find_type_decl(file_id, "unknown_type");
        assert!(decl3.is_none());
    }

    #[test]
    fn test_type_remove() {
        let mut index = create_type_index();
        let file_id = FileId { id: 1 };

        let _ = index.add_type_decl(
            file_id,
            LuaTypeDecl::new(
                file_id,
                TextRange::new(0.into(), 4.into()),
                "new_type".to_string(),
                LuaDeclTypeKind::Class,
                LuaTypeFlag::Partial.into(),
                LuaTypeDeclId::global("new_type"),
            ),
        );

        let decl = index.find_type_decl(file_id, "new_type");
        assert!(decl.is_some());
        index.remove(file_id);
        let decl2 = index.find_type_decl(file_id, "new_type");
        assert!(decl2.is_none());

        let _ = index.add_type_decl(
            file_id,
            LuaTypeDecl::new(
                file_id,
                TextRange::new(0.into(), 4.into()),
                "new_type".to_string(),
                LuaDeclTypeKind::Class,
                LuaTypeFlag::Partial.into(),
                LuaTypeDeclId::global(".new_type"),
            ),
        );

        let file_id2 = FileId { id: 2 };
        let _ = index.add_type_decl(
            file_id2,
            LuaTypeDecl::new(
                file_id2,
                TextRange::new(0.into(), 4.into()),
                "new_type".to_string(),
                LuaDeclTypeKind::Class,
                LuaTypeFlag::Partial.into(),
                LuaTypeDeclId::global("new_type"),
            ),
        );

        let decl = index.find_type_decl(file_id, "new_type");
        assert!(decl.is_some());
        index.remove(file_id);
        let decl2 = index.find_type_decl(file_id2, "new_type");
        assert!(decl2.is_some());
        index.remove(file_id2);
        let decl3 = index.find_type_decl(file_id2, "new_type");
        assert!(decl3.is_none());
    }

    #[test]
    fn test_type_info() {
        let mut index = create_type_index();
        let file_id = FileId { id: 1 };

        let _ = index.add_type_decl(
            file_id,
            LuaTypeDecl::new(
                file_id,
                TextRange::new(0.into(), 4.into()),
                "new_type".to_string(),
                LuaDeclTypeKind::Class,
                LuaTypeFlag::Partial.into(),
                LuaTypeDeclId::global("test.new_type"),
            ),
        );

        let decl = index.find_type_decl(file_id, "test.new_type").unwrap();
        assert_eq!(decl.get_name(), "new_type");
        assert!(decl.is_class());
        assert_eq!(decl.get_namespace(), "test".into());
        assert_eq!(decl.get_full_name(), "test.new_type");
    }
}
