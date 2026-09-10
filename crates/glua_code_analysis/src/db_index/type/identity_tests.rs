use super::*;
use crate::{FileRemap, LuaMemberKey, PositionMap};
use std::collections::BTreeMap;

fn nested_types(inner: LuaType) -> Vec<LuaType> {
    let param = GenericParam::new("T".into(), Some(inner.clone()), None);
    let tpl = Arc::new(GenericTpl::new(
        GenericTplId::Type(0),
        smol_str::SmolStr::new("T").into(),
        Some(inner.clone()),
    ));
    vec![
        LuaType::Instance(Arc::new(LuaInstanceType::new(
            inner.clone(),
            InFiled::new(FileId::new(2), TextRange::new(0.into(), 2.into())),
        ))),
        LuaType::DocAttribute(Arc::new(LuaAttributeType::new(vec![
            ("untyped".into(), None),
            ("value".into(), Some(inner.clone())),
        ]))),
        LuaType::TplRef(tpl.clone()),
        LuaType::ConstTplRef(tpl),
        LuaType::StrTplRef(Arc::new(LuaStringTplType::new(
            "prefix",
            "T",
            GenericTplId::Type(0),
            "suffix",
            Some(inner.clone()),
        ))),
        LuaType::Mapped(Arc::new(LuaMappedType::new(
            (GenericTplId::Type(0), param.clone()),
            LuaType::String,
            true,
            true,
        ))),
        LuaType::Mapped(Arc::new(LuaMappedType::new(
            (
                GenericTplId::Type(0),
                GenericParam::new("T".into(), None, None),
            ),
            inner.clone(),
            false,
            false,
        ))),
        LuaType::Conditional(Arc::new(LuaConditionalType::new(
            LuaType::Boolean,
            LuaType::String,
            LuaType::Number,
            vec![GenericParam::new("U".into(), None, None), param],
            true,
        ))),
        LuaType::Object(Arc::new(LuaObjectType::new_with_fields(
            BTreeMap::from([(LuaMemberKey::ExprType(inner.clone()), LuaType::String)]),
            vec![],
        ))),
        LuaType::Object(Arc::new(LuaObjectType::new_with_fields(
            BTreeMap::new(),
            vec![(inner.clone(), inner.clone())],
        ))),
        LuaType::TableOf(Box::new(inner.clone())),
        LuaType::TableGeneric(Arc::new(vec![LuaType::String, inner.clone()])),
        LuaType::Array(Arc::new(LuaArrayType::from_base_type(inner.clone()))),
        LuaType::Tuple(Arc::new(LuaTupleType::new(
            vec![inner.clone()],
            LuaTupleStatus::InferResolve,
        ))),
        LuaType::Union(Arc::new(LuaUnionType::from_vec(vec![
            inner.clone(),
            LuaType::Nil,
        ]))),
        LuaType::Union(Arc::new(LuaUnionType::from_vec(vec![
            inner.clone(),
            LuaType::String,
        ]))),
        LuaType::Intersection(Arc::new(LuaIntersectionType::new(vec![
            inner.clone(),
            LuaType::String,
        ]))),
        LuaType::MergedTable(Arc::new(LuaMergedTableType::new(vec![inner.clone()]))),
        LuaType::Generic(Arc::new(LuaGenericType::new(
            LuaTypeDeclId::global("Container"),
            vec![inner.clone()],
        ))),
        LuaType::Variadic(Arc::new(VariadicType::Base(inner.clone()))),
        LuaType::Variadic(Arc::new(VariadicType::Multi(vec![inner.clone()]))),
        LuaType::Call(Arc::new(LuaAliasCallType::new(
            LuaAliasCallKind::KeyOf,
            vec![inner.clone()],
        ))),
        LuaType::MultiLineUnion(Arc::new(LuaMultiLineUnion::new(vec![(
            inner.clone(),
            Some("member".into()),
        )]))),
        LuaType::TypeGuard(Arc::new(inner)),
    ]
}

fn identities() -> Vec<LuaType> {
    let range = InFiled::new(FileId::new(1), TextRange::new(2.into(), 4.into()));
    vec![
        LuaType::TableConst(range.clone()),
        LuaType::Signature(LuaSignatureId::new(range.file_id, 2.into())),
        LuaType::Instance(Arc::new(LuaInstanceType::new(LuaType::Table, range))),
    ]
}

#[test]
fn nested_identity_dependencies_are_added_and_removed() {
    let provider = FileId::new(1);
    let consumer = FileId::new(2);
    for identity in identities() {
        for typ in nested_types(identity) {
            let mut index = TypeCacheRefIndex::default();
            index.add(consumer, &typ);
            assert_eq!(
                index.owners_referencing_file(provider).collect::<Vec<_>>(),
                vec![consumer],
                "{typ:?}"
            );
            index.remove(consumer, &typ);
            assert_eq!(
                index.owners_referencing_file(provider).count(),
                0,
                "{typ:?}"
            );
        }
    }
}

#[test]
fn nested_identities_move_revert_and_report_loss() {
    let provider = FileId::new(1);
    let shifted = FileRemap::unvalidated(provider, PositionMap::new("abcdef", "\nabcdef"));
    let reverted = FileRemap::unvalidated(provider, PositionMap::new("\nabcdef", "abcdef"));
    let deleted = FileRemap::unvalidated(provider, PositionMap::new("abcdef", ""));
    let unrelated = FileRemap::identity(FileId::new(3));
    for identity in identities() {
        let moved = remap_identities_in_type(&identity, &shifted).typ.unwrap();
        for (typ, expected) in nested_types(identity).into_iter().zip(nested_types(moved)) {
            let unchanged = remap_identities_in_type(&typ, &unrelated);
            assert!(unchanged.typ.is_none() && !unchanged.lost, "{typ:?}");
            let result = remap_identities_in_type(&typ, &shifted);
            assert!(!result.lost, "{typ:?}");
            assert_eq!(result.typ.as_ref(), Some(&expected), "{typ:?}");
            assert_eq!(
                remap_identities_in_type(&expected, &reverted).typ,
                Some(typ.clone())
            );
            let result = remap_identities_in_type(&typ, &deleted);
            assert!(result.lost && result.typ.is_none(), "{typ:?}");
        }
    }
}
