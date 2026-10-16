mod find_index;
mod find_members;
mod get_member_map;
mod infer_raw_member;

use std::sync::Arc;

use rustc_hash::{FxHashMap, FxHashSet};

use crate::{
    DbIndex, FileId, GmodStateMask, InFiled, LuaDecl, LuaMemberFeature, LuaMemberId, LuaMemberKey,
    LuaMemberOwner, LuaSemanticDeclId, TypeOps,
    db_index::{LuaType, LuaTypeDeclId},
    semantic::type_check::check_type_compact,
};
pub use find_index::find_index_operations;
pub(crate) use find_index::{MetatableIndexField, find_metatable_index_field};
pub use find_members::{
    find_members, find_members_in_workspace_for_file, find_members_in_workspace_for_file_at_offset,
    find_members_with_key, find_members_with_key_in_workspace_for_file,
    find_members_with_key_in_workspace_for_file_at_offset,
};
pub(crate) use find_members::{
    find_structural_members, visible_super_types_in_workspace_for_file_at_offset,
};
pub use get_member_map::{
    get_member_map, get_member_map_in_workspace_for_file,
    get_member_map_in_workspace_for_file_at_offset,
};
use glua_parser::{
    LuaAssignStat, LuaExpr, LuaFuncStat, LuaSyntaxKind, LuaTableExpr, LuaTableField,
};
use glua_parser::{LuaAstNode, LuaIndexExpr};
pub(crate) use infer_raw_member::{
    MemberReadSite, infer_owner_raw_member_type_with_realm,
    infer_owner_raw_member_type_with_realm_cached, resolve_member_item_with_realm,
    source_survives_table_generation_cutoff,
};
pub use infer_raw_member::{infer_raw_member_type, infer_raw_member_type_with_cache};
use rowan::{TextRange, TextSize};

use super::{
    DynamicFieldDefinitionSyntax, InferFailReason, LuaInferCache, SemanticDeclLevel, infer_expr,
    infer_expr_list_value_type_at, infer_node_semantic_decl, infer_table_should_be,
};

pub fn get_buildin_type_map_type_id(type_: &LuaType) -> Option<LuaTypeDeclId> {
    match type_ {
        LuaType::String
        | LuaType::StringConst(_)
        | LuaType::DocStringConst(_)
        | LuaType::Language(_) => Some(LuaTypeDeclId::global("string")),
        LuaType::Io => Some(LuaTypeDeclId::global("io")),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LuaMemberInfo {
    pub property_owner_id: Option<LuaSemanticDeclId>,
    pub key: LuaMemberKey,
    pub typ: LuaType,
    pub feature: Option<LuaMemberFeature>,
    pub overload_index: Option<usize>,
}

type FindMembersResult = Option<Vec<LuaMemberInfo>>;
type RawGetMemberTypeResult = Result<LuaType, InferFailReason>;

pub(crate) fn intersect_member_types(db: &DbIndex, left: LuaType, right: LuaType) -> LuaType {
    if left == right {
        left
    } else {
        TypeOps::Intersect.apply(db, &left, &right)
    }
}

pub(crate) fn merge_open_table_types(db: &DbIndex, types: Vec<LuaType>) -> LuaType {
    let mut table_components = Vec::new();
    let mut other_types = Vec::new();
    let mut seen = FxHashSet::default();

    for typ in types {
        if typ.is_never() {
            continue;
        }

        if let LuaType::Union(union) = &typ {
            let components = union.into_vec();
            if components.iter().all(is_open_table_merge_component) {
                for component in components {
                    if seen.insert(component.clone()) {
                        table_components.push(component);
                    }
                }
            } else if seen.insert(typ.clone()) {
                other_types.push(typ);
            }
            continue;
        }

        if let LuaType::MergedTable(merged) = &typ {
            for component in merged.get_types() {
                if seen.insert(component.clone()) {
                    table_components.push(component.clone());
                }
            }
            continue;
        }

        if is_open_table_merge_component(&typ) {
            if seen.insert(typ.clone()) {
                table_components.push(typ);
            }
        } else if seen.insert(typ.clone()) {
            other_types.push(typ);
        }
    }

    let table_type = merge_open_table_components(table_components);
    let mut result = table_type;
    for typ in other_types {
        result = match result {
            Some(existing) => Some(TypeOps::Union.apply(db, &existing, &typ)),
            None => Some(typ),
        };
    }

    result.unwrap_or(LuaType::Never)
}

pub(crate) fn local_class_table_member_ids(
    db: &DbIndex,
    type_id: &LuaTypeDeclId,
    member_key: &LuaMemberKey,
) -> Vec<LuaMemberId> {
    let Some(type_decl) = db.get_type_index().get_type_decl(type_id) else {
        return Vec::new();
    };
    if !type_decl.is_class() {
        return Vec::new();
    }

    // Only declarations bound to this class can back it with a table
    // literal. The type index tracks those bindings on every type-cache
    // write, so resolving them is a lookup of the class's own declarations
    // instead of a scan over every declaration in the class's files.
    //
    // The class's files bound where the tables may live; binds outside them
    // cannot contribute, so the lookup only reads those (type, file) slots.
    let class_files: FxHashSet<FileId> = type_decl
        .get_location_flags()
        .map(|(file_id, _)| file_id)
        .collect();
    let bound_decls: Vec<crate::LuaDeclId> = db
        .get_type_index()
        .decls_bound_to_type(type_id, &class_files)
        .collect();
    if bound_decls.is_empty() {
        return Vec::new();
    }

    let mut decls_by_file: FxHashMap<FileId, Vec<crate::LuaDeclId>> = FxHashMap::default();
    for decl_id in bound_decls {
        decls_by_file
            .entry(decl_id.file_id)
            .or_default()
            .push(decl_id);
    }
    if decls_by_file.is_empty() {
        return Vec::new();
    }
    let mut files: Vec<FileId> = decls_by_file.keys().copied().collect();
    files.sort_by_key(|file_id| file_id.id);

    let member_index = db.get_member_index();
    let mut member_ids = Vec::new();
    for file_id in files {
        let Some(decl_ids) = decls_by_file.get(&file_id) else {
            continue;
        };
        for decl_id in decl_ids {
            let Some(decl) = db.get_decl_index().get_decl(decl_id) else {
                continue;
            };
            let Some(owner) = local_table_decl_member_owner(decl) else {
                continue;
            };
            let Some(member_item) = member_index.get_member_item(&owner, member_key) else {
                continue;
            };
            member_ids.extend(
                member_item
                    .get_member_ids()
                    .into_iter()
                    .filter(|member_id| {
                        member_index
                            .get_member(member_id)
                            .is_some_and(|member| member.get_key() == member_key)
                    }),
            );
        }
    }

    member_ids.sort_by_key(|member_id| {
        let syntax_id = member_id.get_syntax_id();
        (
            member_id.file_id.id,
            u32::from(member_id.get_position()),
            u32::from(syntax_id.get_range().end()),
            syntax_id.get_kind() as u16,
        )
    });
    member_ids.dedup();
    member_ids
}

/// Revision-gated memo for [`local_class_table_member_ids`].
///
/// The answer derives from the type-bound-decl map plus the live member
/// slots, both covered by `DbIndex::type_structure_revision`. A hit is
/// returned only when the entry's revision still matches; any mutable
/// type/member/signature/module write, remove or clear bumps the revision
/// and forces a recompute, so mutable analysis never observes a stale hit.
/// Within the immutable diagnostics phase the revision is fixed and repeated
/// reads of one class member share a single scan.
pub(crate) fn cached_local_class_table_member_ids(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    type_id: &LuaTypeDeclId,
    member_key: &LuaMemberKey,
) -> Arc<Vec<LuaMemberId>> {
    let revision = db.type_structure_revision();
    let cache_key = (type_id.clone(), member_key.clone());
    if let Some((cached_revision, member_ids)) = cache.tracked_get(
        |cache| &mut cache.local_class_table_member_ids_cache,
        &cache_key,
    ) && cached_revision == revision
    {
        return member_ids;
    }

    let frame = crate::db_index::derivation::open_frame();
    let member_ids = Arc::new(local_class_table_member_ids(db, type_id, member_key));
    cache.tracked_insert(
        |cache| &mut cache.local_class_table_member_ids_cache,
        cache_key,
        (revision, Arc::clone(&member_ids)),
        frame,
    );
    member_ids
}

fn local_table_decl_member_owner(decl: &LuaDecl) -> Option<LuaMemberOwner> {
    let expr_id = decl.table_literal_initializer()?;
    Some(LuaMemberOwner::Element(InFiled::new(
        decl.get_id().file_id,
        expr_id.get_range(),
    )))
}

fn merge_open_table_components(mut components: Vec<LuaType>) -> Option<LuaType> {
    if components
        .iter()
        .any(|component| !matches!(component, LuaType::Table))
    {
        components.retain(|component| !matches!(component, LuaType::Table));
    }

    match components.as_slice() {
        [] => None,
        [only] => Some(only.clone()),
        _ => Some(crate::LuaMergedTableType::new(components).into()),
    }
}

fn is_open_table_merge_component(typ: &LuaType) -> bool {
    matches!(
        typ,
        LuaType::Table
            | LuaType::TableConst(_)
            | LuaType::Object(_)
            | LuaType::MergedTable(_)
            | LuaType::TableOf(_)
    )
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use crate::{
        DbIndex, LuaInferCache, LuaMemberKey, LuaObjectType, LuaType, LuaTypeDeclId,
        VirtualWorkspace,
    };

    use rowan::{TextRange, TextSize};

    use crate::{FileId, InFiled, LuaMemberOwner};

    use super::{
        cached_local_class_table_member_ids, infer_owner_raw_member_type_with_realm_cached,
        local_class_table_member_ids, member_key_as_type, member_key_matches_type,
        merge_open_table_types,
    };

    fn table_owner(file_id: FileId, content: &str) -> LuaMemberOwner {
        let start = content.find('{').expect("a table literal") as u32;
        let end = content.find('}').expect("a table literal") as u32 + 1;
        LuaMemberOwner::Element(InFiled::new(
            file_id,
            TextRange::new(start.into(), end.into()),
        ))
    }

    /// A read before a same-file write and one after it each take the answer
    /// at their own offset, though they share one cache.
    #[test]
    fn a_same_file_member_is_read_at_each_offset_through_one_cache() {
        const CONTENT: &str = r#"
            local t = {}
            local before = t.x
            t.x = 1
            local after = t.x
        "#;
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def_file("lua/autorun/shared/order.lua", CONTENT);
        let db = ws.analysis.compilation.get_db();
        let owner = table_owner(file_id, CONTENT);
        let key = LuaMemberKey::Name("x".into());
        let before = TextSize::from(CONTENT.find("t.x").expect("the read before") as u32);
        let after = TextSize::from(CONTENT.rfind("t.x").expect("the read after") as u32);
        let read = |cache: &mut LuaInferCache, position| {
            infer_owner_raw_member_type_with_realm_cached(
                db,
                cache,
                owner.clone(),
                &key,
                file_id,
                Some(position),
                None,
            )
        };

        let mut cache = LuaInferCache::new(file_id, Default::default());
        let shared = [read(&mut cache, before), read(&mut cache, after)];
        let fresh = [before, after].map(|position| {
            read(
                &mut LuaInferCache::new(file_id, Default::default()),
                position,
            )
        });

        assert_ne!(fresh[0], fresh[1], "the write lands between the reads");
        assert_eq!(shared, fresh);
    }

    /// A member another file writes reads the same at every offset in one
    /// realm, so one answer serves them all.
    #[test]
    fn a_member_another_file_writes_is_resolved_once_per_realm() {
        const LIBRARY: &str = r#"
            Config = { size = 1 }
        "#;
        const CALLER: &str = r#"
            local a = Config.size
            local b = Config.size
        "#;
        let mut ws = VirtualWorkspace::new();
        let library = ws.def_file("lua/autorun/shared/config.lua", LIBRARY);
        let caller = ws.def_file("lua/autorun/shared/use.lua", CALLER);
        let db = ws.analysis.compilation.get_db();
        let owner = table_owner(library, LIBRARY);
        let key = LuaMemberKey::Name("size".into());
        let mut cache = LuaInferCache::new(caller, Default::default());

        let reads = [CALLER.find("Config"), CALLER.rfind("Config")].map(|offset| {
            let position = TextSize::from(offset.expect("a read") as u32);
            infer_owner_raw_member_type_with_realm_cached(
                db,
                &mut cache,
                owner.clone(),
                &key,
                caller,
                Some(position),
                None,
            )
        });

        assert!(reads[0].is_ok(), "the member resolves: {:?}", reads[0]);
        assert_eq!(reads[0], reads[1]);
        assert_eq!(cache.owner_raw_member_type_cache.len(), 1);
    }

    /// A keyed read of `_G` finds the declarations of that global the full
    /// scan finds, in the same order.
    #[test]
    fn a_keyed_global_read_finds_what_the_global_scan_finds() {
        let mut ws = VirtualWorkspace::new();
        ws.def_file(
            "lua/autorun/shared/b.lua",
            r#"
                Shared = 2
                Other = true
            "#,
        );
        ws.def_file("lua/autorun/shared/a.lua", "Shared = 1");
        let db = ws.analysis.compilation.get_db();
        let key = LuaMemberKey::Name("Shared".into());

        let scanned = crate::find_members(db, &LuaType::Global)
            .expect("globals are found")
            .into_iter()
            .filter(|member| member.key == key)
            .collect::<Vec<_>>();
        let keyed = crate::find_members_with_key(db, &LuaType::Global, key.clone(), true)
            .expect("the global is found");

        assert_eq!(scanned.len(), 2, "both declarations of the global");
        assert_eq!(keyed, scanned);
    }

    /// A primitive access decides literal keys without the type check, and
    /// answers as the check would.
    #[test]
    fn a_primitive_access_matches_literal_keys_as_the_type_check_does() {
        let db = DbIndex::new();
        let accesses = [
            LuaType::Any,
            LuaType::Unknown,
            LuaType::Integer,
            LuaType::Number,
            LuaType::String,
            LuaType::Boolean,
        ];
        let keys = [LuaMemberKey::Integer(3), LuaMemberKey::Name("name".into())];
        for access in &accesses {
            for key in &keys {
                let key_type = member_key_as_type(key).expect("a literal key has a type");
                let checked = crate::check_type_compact(&db, access, &key_type).is_ok()
                    || crate::check_type_compact(&db, &key_type, access).is_ok();
                assert_eq!(
                    member_key_matches_type(&db, access, key),
                    checked,
                    "{access:?} against {key:?}",
                );
            }
        }
    }

    #[test]
    fn literal_union_keys_match_only_their_own_members() {
        let db = DbIndex::new();
        let text = |value: &str| LuaType::DocStringConst(smol_str::SmolStr::new(value).into());
        let access = LuaType::from_vec(vec![text("a"), text("b")]);
        for (key, expected) in [
            (LuaMemberKey::Name("a".into()), true),
            (LuaMemberKey::Name("b".into()), true),
            (LuaMemberKey::Name("callback".into()), false),
            (LuaMemberKey::Integer(1), false),
            (LuaMemberKey::ExprType(LuaType::String), true),
            (LuaMemberKey::ExprType(LuaType::Table), false),
            (
                LuaMemberKey::ExprType(LuaType::from_vec(vec![text("b"), text("c")])),
                true,
            ),
            (
                LuaMemberKey::ExprType(LuaType::from_vec(vec![text("c"), text("d")])),
                false,
            ),
        ] {
            assert_eq!(
                member_key_matches_type(&db, &access, &key),
                expected,
                "{key:?}"
            );
        }

        let access = LuaType::from_vec(vec![LuaType::IntegerConst(1), LuaType::DocIntegerConst(2)]);
        assert!(member_key_matches_type(
            &db,
            &access,
            &LuaMemberKey::Integer(1)
        ));
        assert!(!member_key_matches_type(
            &db,
            &access,
            &LuaMemberKey::Integer(3)
        ));
    }

    #[test]
    fn typed_keys_preserve_domains_and_unknown_key_fallbacks() {
        let db = DbIndex::new();
        let numeric = LuaMemberKey::ExprType(LuaType::Integer);
        let table = LuaMemberKey::ExprType(LuaType::Table);
        let unknown = LuaMemberKey::ExprType(LuaType::Unknown);
        assert!(member_key_matches_type(&db, &LuaType::Integer, &numeric));
        assert!(!member_key_matches_type(&db, &LuaType::Integer, &table));
        assert!(member_key_matches_type(&db, &LuaType::Table, &table));
        assert!(!member_key_matches_type(&db, &LuaType::Table, &numeric));
        let either = LuaType::from_vec(vec![LuaType::Integer, LuaType::Table]);
        assert!(member_key_matches_type(&db, &either, &numeric));
        assert!(member_key_matches_type(&db, &either, &table));
        assert!(member_key_matches_type(&db, &LuaType::Unknown, &numeric));
        assert!(member_key_matches_type(&db, &LuaType::Unknown, &table));
        assert!(member_key_matches_type(&db, &LuaType::Integer, &unknown));
        assert!(!member_key_matches_type(
            &db,
            &LuaType::DocIntegerConst(3),
            &unknown
        ));
    }

    #[test]
    fn merge_open_table_types_keeps_tableof_as_table_component() {
        let db = DbIndex::new();
        let table_of = LuaType::TableOf(Box::new(LuaType::Ref(LuaTypeDeclId::global("Entity"))));

        let mut fields = BTreeMap::new();
        fields.insert(LuaMemberKey::Name("flags".into()), LuaType::Table);
        let object = LuaType::Object(LuaObjectType::new_with_fields(fields, Vec::new()).into());

        let merged = merge_open_table_types(&db, vec![table_of.clone(), object.clone()]);
        let LuaType::MergedTable(merged_table) = merged else {
            panic!("expected tableof and object fragments to merge as an open table");
        };

        assert_eq!(merged_table.get_types(), &[table_of, object]);
    }

    #[test]
    fn merge_open_table_types_keeps_mixed_union_alternatives_distinct() {
        let db = DbIndex::new();
        let mut left_fields = BTreeMap::new();
        left_fields.insert(LuaMemberKey::Name("left".into()), LuaType::Integer);
        let left = LuaType::Object(LuaObjectType::new_with_fields(left_fields, Vec::new()).into());
        let mut right_fields = BTreeMap::new();
        right_fields.insert(LuaMemberKey::Name("right".into()), LuaType::String);
        let right =
            LuaType::Object(LuaObjectType::new_with_fields(right_fields, Vec::new()).into());
        let mixed = LuaType::from_vec_structural(vec![left, right, LuaType::BooleanConst(false)]);

        assert_eq!(merge_open_table_types(&db, vec![mixed.clone()]), mixed);
    }

    /// A table literal bound to a class backs it while its file is open, stops
    /// when the file is deleted, and backs it again once the file reopens, as
    /// a cold build of the reopened workspace does.
    #[test]
    fn local_class_table_lookup_follows_delete_and_reopen() {
        use crate::VirtualWorkspace;

        use super::local_class_table_member_ids;

        const BOUND: &str = r#"
            ---@class BoundClass

            ---@type BoundClass
            local instance = { speed = 10 }
        "#;
        let path = "lua/autorun/shared/bound.lua";
        let bound = LuaTypeDeclId::global("BoundClass");
        let speed = LuaMemberKey::Name("speed".into());
        let mut ws = VirtualWorkspace::new();
        let uri = ws.virtual_url_generator.new_uri(path);
        ws.def_file(path, BOUND);
        let db = ws.analysis.compilation.get_db();
        assert_eq!(local_class_table_member_ids(db, &bound, &speed).len(), 1);

        ws.analysis.remove_file_by_uri(&uri);
        let db = ws.analysis.compilation.get_db();
        assert!(local_class_table_member_ids(db, &bound, &speed).is_empty());

        ws.def_file(path, BOUND);
        let mut cold = VirtualWorkspace::new();
        cold.def_file(path, BOUND);
        let reopened =
            local_class_table_member_ids(ws.analysis.compilation.get_db(), &bound, &speed);
        let built =
            local_class_table_member_ids(cold.analysis.compilation.get_db(), &bound, &speed);
        assert_eq!(reopened.len(), 1, "reopened {reopened:?}, cold {built:?}");
        fn sites(ids: &[crate::LuaMemberId]) -> Vec<glua_parser::LuaSyntaxId> {
            ids.iter().map(|id| *id.get_syntax_id()).collect()
        }
        assert_eq!(sites(&reopened), sites(&built));
        assert_eq!(ws.semantic_digest(), cold.semantic_digest());
    }

    /// A field of a class's table literal in one of the class's files types a
    /// read through the class in another file.
    #[test]
    fn local_class_table_member_types_a_read_in_another_file() {
        use crate::VirtualWorkspace;

        use super::local_class_table_member_ids;

        let mut ws = VirtualWorkspace::new();
        ws.def_files(vec![
            (
                "lua/autorun/shared/sh_radio.lua",
                r#"
                ---@class RadioPreset
                "#,
            ),
            (
                "lua/autorun/shared/sh_radio_presets.lua",
                r#"
                ---@class RadioPreset

                ---@type RadioPreset
                local preset = { volume = 1 }
                "#,
            ),
        ]);
        let reader = ws.def_file(
            "lua/autorun/client/cl_radio.lua",
            r#"
            ---@param preset RadioPreset
            local function apply(preset)
                local volume = preset.volume
                print(volume)
            end
            "#,
        );
        let preset = LuaTypeDeclId::global("RadioPreset");
        let volume = LuaMemberKey::Name("volume".into());
        assert_eq!(
            local_class_table_member_ids(ws.analysis.compilation.get_db(), &preset, &volume).len(),
            1
        );
        assert_eq!(ws.humanize_type(ws.local_ty(reader, "volume")), "1");
    }

    /// A local without a table literal settled to a class by its uses is not
    /// a table backing the class: the class's bindings do not move.
    #[test]
    fn a_local_settled_to_a_class_without_a_table_literal_binds_nothing() {
        use crate::db_index::derivation::{ABSENT, FactKey};
        use crate::{Emmyrc, VirtualWorkspace};

        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        let file_id = ws.def_file(
            "lua/autorun/client/cl_tuner.lua",
            r#"
            ---@class Entity
            ---@class Radio: Entity
            ---@field Tune fun(self: Radio)

            ---@type Entity
            local tuned
            tuned:Tune()
            print(tuned)
            "#,
        );
        assert_eq!(ws.humanize_type(ws.local_ty(file_id, "tuned")), "Radio");
        let db = ws.analysis.compilation.get_db();
        let radio = LuaTypeDeclId::global("Radio");
        let files = rustc_hash::FxHashSet::from_iter([file_id]);
        assert_eq!(
            db.get_type_index()
                .decls_bound_to_type(&radio, &files)
                .count(),
            0
        );
        assert_eq!(
            db.fact_fingerprint(&FactKey::BoundDecls(radio, file_id)),
            ABSENT
        );
    }

    /// The class→table-owner lookup answers the live decl bindings: editing
    /// the annotation that binds a local table to a class moves the table's
    /// members to the newly bound class without any cache to invalidate.
    #[test]
    fn local_class_table_lookup_follows_decl_rebinds_across_edits() {
        use crate::VirtualWorkspace;

        use super::local_class_table_member_ids;

        const INITIAL: &str = r#"
            ---@class BoundClass
            ---@class OtherClass

            ---@type BoundClass
            local instance = { speed = 10 }

            ---@type OtherClass
            local other = { power = 5 }
        "#;
        const REBOUND: &str = r#"
            ---@class BoundClass
            ---@class OtherClass

            ---@type OtherClass
            local instance = { speed = 10 }

            ---@type OtherClass
            local other = { power = 5 }
        "#;

        let mut ws = VirtualWorkspace::new();
        let uri = ws
            .virtual_url_generator
            .new_uri("lua/autorun/shared/bound.lua");
        ws.analysis
            .update_file_by_uri(&uri, Some(INITIAL.to_string()))
            .map(|(id, _)| id)
            .expect("initial file indexed");
        let bound = LuaTypeDeclId::global("BoundClass");
        let other = LuaTypeDeclId::global("OtherClass");
        let speed = LuaMemberKey::Name("speed".into());
        let db = ws.analysis.compilation.get_db();
        assert_eq!(
            local_class_table_member_ids(db, &bound, &speed).len(),
            1,
            "the table bound to BoundClass backs it",
        );
        assert!(
            local_class_table_member_ids(db, &other, &speed).is_empty(),
            "no table bound to OtherClass carries speed yet",
        );

        ws.analysis
            .update_file_by_uri(&uri, Some(REBOUND.to_string()))
            .map(|(id, _)| id)
            .expect("rebound file indexed");
        let db = ws.analysis.compilation.get_db();
        assert!(
            local_class_table_member_ids(db, &bound, &speed).is_empty(),
            "rebinding the only table away leaves BoundClass with no backer",
        );
        assert_eq!(
            local_class_table_member_ids(db, &other, &speed).len(),
            1,
            "the rebound table now backs OtherClass",
        );
    }

    fn assert_cached_matches_uncached(
        ws: &VirtualWorkspace,
        cache: &mut LuaInferCache,
        type_id: &LuaTypeDeclId,
        key: &LuaMemberKey,
    ) -> Vec<crate::LuaMemberId> {
        let db = ws.analysis.compilation.get_db();
        let uncached = local_class_table_member_ids(db, type_id, key);
        let cached = cached_local_class_table_member_ids(db, cache, type_id, key);
        assert_eq!(
            cached.as_ref(),
            &uncached,
            "cached and uncached agree for {}::{:?}",
            type_id.get_name(),
            key,
        );
        uncached
    }

    #[test]
    fn cached_local_class_table_ids_reuse_hit_and_stay_sorted() {
        const CONTENT: &str = r#"
            ---@class SortedClass
            ---@type SortedClass
            local first = { speed = 1 }
            ---@type SortedClass
            local second = { speed = 2 }
        "#;

        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def_file("lua/autorun/shared/sorted.lua", CONTENT);
        let type_id = LuaTypeDeclId::global("SortedClass");
        let key = LuaMemberKey::Name("speed".into());
        let mut cache = LuaInferCache::new(file_id, Default::default());

        let db = ws.analysis.compilation.get_db();
        let uncached = local_class_table_member_ids(db, &type_id, &key);
        assert_eq!(uncached.len(), 2, "both bound tables back the class");
        let first_hit = cached_local_class_table_member_ids(db, &mut cache, &type_id, &key);
        assert_eq!(first_hit.as_ref(), &uncached);
        let second_hit = cached_local_class_table_member_ids(db, &mut cache, &type_id, &key);
        assert!(
            Arc::ptr_eq(&first_hit, &second_hit),
            "unchanged query reuses the cached allocation",
        );
        assert_eq!(second_hit.as_ref(), &uncached);
        let positions: Vec<_> = second_hit.iter().map(|id| id.get_position()).collect();
        assert!(
            positions.windows(2).all(|w| w[0] <= w[1]),
            "repeated query returns the same sorted ids: {:?}",
            positions,
        );
    }

    #[test]
    fn cached_local_class_table_ids_see_first_member_insertion() {
        const EMPTY: &str = r#"
            ---@class GrowingClass
            ---@type GrowingClass
            local box = {}
        "#;
        const FILLED: &str = r#"
            ---@class GrowingClass
            ---@type GrowingClass
            local box = { speed = 1 }
        "#;

        let mut ws = VirtualWorkspace::new();
        let path = "lua/autorun/shared/growing.lua";
        let file_id = ws.def_file(path, EMPTY);
        let type_id = LuaTypeDeclId::global("GrowingClass");
        let key = LuaMemberKey::Name("speed".into());
        let mut cache = LuaInferCache::new(file_id, Default::default());

        let found = assert_cached_matches_uncached(&ws, &mut cache, &type_id, &key);
        assert!(found.is_empty(), "no table field backs the key yet");

        let uri = ws.virtual_url_generator.new_uri(path);
        ws.analysis
            .update_file_by_uri(&uri, Some(FILLED.to_string()))
            .expect("filled file indexed");
        let found = assert_cached_matches_uncached(&ws, &mut cache, &type_id, &key);
        assert_eq!(
            found.len(),
            1,
            "the cached empty hit is invalidated by the member insertion",
        );
    }

    #[test]
    fn cached_local_class_table_ids_follow_rekey_rehome_and_removal() {
        const SPEED: &str = r#"
            ---@class RekeyClass
            ---@type RekeyClass
            local item = { speed = 1 }
        "#;
        const REKEYED: &str = r#"
            ---@class RekeyClass
            ---@type RekeyClass
            local item = { velocity = 1 }
        "#;
        const SHIFTED: &str = r#"
            ---@class RekeyClass
            -- shifting the table down rehomes its members to a new owner range
            ---@type RekeyClass
            local item = { velocity = 1 }
        "#;
        const REMOVED: &str = r#"
            ---@class RekeyClass
            ---@type RekeyClass
            local item = {}
        "#;

        let mut ws = VirtualWorkspace::new();
        let path = "lua/autorun/shared/rekey.lua";
        let file_id = ws.def_file(path, SPEED);
        let type_id = LuaTypeDeclId::global("RekeyClass");
        let speed = LuaMemberKey::Name("speed".into());
        let velocity = LuaMemberKey::Name("velocity".into());
        let mut cache = LuaInferCache::new(file_id, Default::default());

        let found = assert_cached_matches_uncached(&ws, &mut cache, &type_id, &speed);
        assert_eq!(found.len(), 1, "initial hit populates the cache");

        let uri = ws.virtual_url_generator.new_uri(path);
        ws.analysis
            .update_file_by_uri(&uri, Some(REKEYED.to_string()))
            .expect("rekeyed file indexed");
        let found = assert_cached_matches_uncached(&ws, &mut cache, &type_id, &speed);
        assert!(
            found.is_empty(),
            "the rekeyed-away member is gone after the hit",
        );
        let found = assert_cached_matches_uncached(&ws, &mut cache, &type_id, &velocity);
        assert_eq!(found.len(), 1, "the rekeyed-to member appears");

        ws.analysis
            .update_file_by_uri(&uri, Some(SHIFTED.to_string()))
            .expect("shifted file indexed");
        let found = assert_cached_matches_uncached(&ws, &mut cache, &type_id, &velocity);
        assert_eq!(
            found.len(),
            1,
            "rehoming the table keeps the member visible through the cache",
        );

        ws.analysis
            .update_file_by_uri(&uri, Some(REMOVED.to_string()))
            .expect("emptied file indexed");
        let found = assert_cached_matches_uncached(&ws, &mut cache, &type_id, &velocity);
        assert!(
            found.is_empty(),
            "removing the field after a hit reads empty again",
        );
    }

    #[test]
    fn cached_local_class_table_ids_handle_multiple_decls_across_files() {
        const FILE_A: &str = r#"
            ---@class MultiClass
            ---@type MultiClass
            local a = { speed = 1 }
            ---@type MultiClass
            local b = { speed = 2 }
        "#;
        const FILE_B: &str = r#"
            ---@class MultiClass
            ---@type MultiClass
            local c = { speed = 3 }
        "#;
        const FILE_B_UNBOUND: &str = r#"
            ---@class MultiClass
            ---@class OtherMulti
            ---@type OtherMulti
            local c = { speed = 3 }
        "#;

        let mut ws = VirtualWorkspace::new();
        let path_a = "lua/autorun/shared/multi_a.lua";
        let path_b = "lua/autorun/shared/multi_b.lua";
        let file_a = ws.def_file(path_a, FILE_A);
        ws.def_file(path_b, FILE_B);
        let type_id = LuaTypeDeclId::global("MultiClass");
        let key = LuaMemberKey::Name("speed".into());
        let mut cache = LuaInferCache::new(file_a, Default::default());

        let found = assert_cached_matches_uncached(&ws, &mut cache, &type_id, &key);
        assert_eq!(
            found.len(),
            3,
            "two bound decls in one file plus one across files",
        );
        let order: Vec<_> = found
            .iter()
            .map(|id| (id.file_id, id.get_position()))
            .collect();
        let mut sorted = order.clone();
        sorted.sort();
        assert_eq!(order, sorted, "ids stay in deterministic sorted order");

        let uri_b = ws.virtual_url_generator.new_uri(path_b);
        ws.analysis
            .update_file_by_uri(&uri_b, Some(FILE_B_UNBOUND.to_string()))
            .expect("unbound file indexed");
        let found = assert_cached_matches_uncached(&ws, &mut cache, &type_id, &key);
        assert_eq!(
            found.len(),
            2,
            "unbinding the cross-file table leaves the single-file pair",
        );
    }

    #[test]
    fn cached_local_class_table_ids_ignore_non_table_and_multivalue_initializers() {
        const CONTENT: &str = r#"
            ---@class FilteredClass
            ---@type FilteredClass
            local nontable = 42
            ---@type FilteredClass
            local good = { wanted = 1 }
            local function make() return 1 end
            ---@type FilteredClass
            local x, y = make()
        "#;

        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def_file("lua/autorun/shared/filtered.lua", CONTENT);
        let type_id = LuaTypeDeclId::global("FilteredClass");
        let wanted = LuaMemberKey::Name("wanted".into());
        let mut cache = LuaInferCache::new(file_id, Default::default());

        let found = assert_cached_matches_uncached(&ws, &mut cache, &type_id, &wanted);
        assert_eq!(
            found.len(),
            1,
            "only the table literal backs the class; the number and the call decls do not",
        );
    }

    #[test]
    fn cached_local_class_table_ids_follow_decl_bind_and_clear_drops() {
        const BOUND: &str = r#"
            ---@class CachedBound
            ---@class CachedOther
            ---@type CachedBound
            local instance = { speed = 10 }
        "#;
        const REBOUND: &str = r#"
            ---@class CachedBound
            ---@class CachedOther
            ---@type CachedOther
            local instance = { speed = 10 }
        "#;

        let mut ws = VirtualWorkspace::new();
        let path = "lua/autorun/shared/cached_bound.lua";
        let file_id = ws.def_file(path, BOUND);
        let bound = LuaTypeDeclId::global("CachedBound");
        let other = LuaTypeDeclId::global("CachedOther");
        let speed = LuaMemberKey::Name("speed".into());
        let mut cache = LuaInferCache::new(file_id, Default::default());

        let found = assert_cached_matches_uncached(&ws, &mut cache, &bound, &speed);
        assert_eq!(found.len(), 1, "bound table backs its class");
        let found = assert_cached_matches_uncached(&ws, &mut cache, &other, &speed);
        assert!(found.is_empty(), "no table backs the other class yet");

        let uri = ws.virtual_url_generator.new_uri(path);
        ws.analysis
            .update_file_by_uri(&uri, Some(REBOUND.to_string()))
            .expect("rebound file indexed");
        let found = assert_cached_matches_uncached(&ws, &mut cache, &bound, &speed);
        assert!(
            found.is_empty(),
            "rebinding away invalidates the earlier hit",
        );
        let found = assert_cached_matches_uncached(&ws, &mut cache, &other, &speed);
        assert_eq!(found.len(), 1, "the rebound table backs the other class");

        cache.clear();
        assert!(
            cache.local_class_table_member_ids_cache.is_empty(),
            "full clear drops the entries",
        );
        let found = assert_cached_matches_uncached(&ws, &mut cache, &other, &speed);
        assert_eq!(found.len(), 1, "lookup still agrees after a clear");
    }
}

#[derive(Debug, Clone)]
pub(crate) struct DynamicFieldResolution {
    pub typ: LuaType,
    pub semantic_decl: Option<LuaSemanticDeclId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DynamicFieldDefinitionVisibility {
    Runtime,
    ShapeOnly,
}

struct VisibleDynamicFieldDefinition {
    member_id: Option<LuaMemberId>,
    visibility: DynamicFieldDefinitionVisibility,
}

pub(crate) fn resolve_dynamic_field_member(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    prefix_type: &LuaType,
    member_key: &LuaMemberKey,
    access_position: Option<TextSize>,
) -> Result<Option<DynamicFieldResolution>, InferFailReason> {
    if !db.get_emmyrc().gmod.enabled
        || !db.get_emmyrc().gmod.infer_dynamic_fields
        || !cache.dynamic_fields_visible()
    {
        if db.get_emmyrc().gmod.enabled
            && db.get_emmyrc().gmod.infer_dynamic_fields
            && !cache.dynamic_fields_visible()
        {
            if let Some(field_name) = member_key.get_name() {
                record_dynamic_field_read_for_prefix(db, prefix_type, field_name);
            }
        }
        return Ok(None);
    }

    let cache_key = (prefix_type.clone(), member_key.clone(), access_position);
    if let Some(cached) = cache.tracked_get(
        |cache| &mut cache.dynamic_field_resolution_cache,
        &cache_key,
    ) {
        return Ok(cached.map(|(typ, semantic_decl)| DynamicFieldResolution { typ, semantic_decl }));
    }

    let Some(field_name) = member_key.get_name() else {
        return Ok(None);
    };
    let frame = crate::db_index::derivation::open_frame();
    let resolved =
        resolve_dynamic_field_uncached(db, cache, prefix_type, field_name, access_position);
    cache.tracked_insert(
        |cache| &mut cache.dynamic_field_resolution_cache,
        cache_key,
        resolved.clone(),
        frame,
    );
    Ok(resolved.map(|(typ, semantic_decl)| DynamicFieldResolution { typ, semantic_decl }))
}

fn resolve_dynamic_field_uncached(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    prefix_type: &LuaType,
    field_name: &str,
    access_position: Option<TextSize>,
) -> Option<(LuaType, Option<LuaSemanticDeclId>)> {
    let caller_file_id = cache.get_file_id();
    let definitions = dynamic_field_definitions(
        db,
        cache,
        caller_file_id,
        prefix_type,
        field_name,
        access_position,
    );
    if definitions.is_empty() {
        return None;
    }
    Some(dynamic_field_definitions_type(
        db,
        cache,
        definitions,
        false,
    ))
}

/// What `field_name` holds on `prefix_type` at `access_position`, from every
/// dynamic writer but those `skip` names: `None` when it names none of them,
/// and an inner `None` when no other writer is left.
pub(crate) fn dynamic_field_type_without(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    prefix_type: &LuaType,
    field_name: &str,
    access_position: TextSize,
    skip: impl Fn(LuaMemberId) -> bool,
) -> Option<Option<LuaType>> {
    if !db.get_emmyrc().gmod.enabled || !db.get_emmyrc().gmod.infer_dynamic_fields {
        return None;
    }
    let caller_file_id = cache.get_file_id();
    let (skipped, kept): (Vec<_>, Vec<_>) = dynamic_field_definitions(
        db,
        cache,
        caller_file_id,
        prefix_type,
        field_name,
        Some(access_position),
    )
    .into_iter()
    .partition(|definition| definition.member_id.is_some_and(&skip));
    if skipped.is_empty() {
        return None;
    }
    if kept.is_empty() {
        return Some(None);
    }
    Some(Some(
        dynamic_field_definitions_type(db, cache, kept, true).0,
    ))
}

/// The type the dynamic writers `definitions` give a field, with the member
/// the first of them names. `informative_only` leaves out writers whose value
/// is `any` or `unknown`, which say nothing of the field. A writer whose value
/// is unresolved adds no type while another writer has one, as a member
/// slot's untyped writers do not: otherwise the floor a cycle of writers took
/// before one of them landed stays in the field for good.
fn dynamic_field_definitions_type(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    definitions: Vec<VisibleDynamicFieldDefinition>,
    informative_only: bool,
) -> (LuaType, Option<LuaSemanticDeclId>) {
    let mut member_types = Vec::new();
    let mut semantic_decl = None;
    let mut has_runtime_type = false;
    for definition in definitions {
        let Some(member_id) = definition.member_id else {
            continue;
        };
        if semantic_decl.is_none() {
            semantic_decl = Some(LuaSemanticDeclId::Member(member_id));
        }
        if let Some(typ) = dynamic_field_member_type(db, cache, &member_id)
            && !(informative_only && matches!(typ, LuaType::Any | LuaType::Unknown))
        {
            member_types.push(typ);
            if definition.visibility == DynamicFieldDefinitionVisibility::Runtime {
                has_runtime_type = true;
            }
        }
    }
    if member_types.iter().any(|typ| !typ.is_unknown()) {
        member_types.retain(|typ| !typ.is_unknown());
    }

    if !member_types.is_empty() && !has_runtime_type {
        member_types.push(LuaType::Nil);
    }

    let typ = match member_types.as_slice() {
        // Finding no contributing member says nothing about what the field
        // holds, so the honest answer is unresolved rather than unconstrained.
        [] => LuaType::Unknown,
        [only] => only.clone(),
        _ => LuaType::from_vec(member_types),
    };
    (typ, semantic_decl)
}

pub(crate) fn resolve_dynamic_field_member_for_file(
    db: &DbIndex,
    caller_file_id: FileId,
    prefix_type: &LuaType,
    member_key: &LuaMemberKey,
) -> Option<DynamicFieldResolution> {
    let mut cache = LuaInferCache::new(caller_file_id, Default::default());
    resolve_dynamic_field_member(db, &mut cache, prefix_type, member_key, None).unwrap_or_default()
}

fn dynamic_field_member_type(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    member_id: &LuaMemberId,
) -> Option<LuaType> {
    if let Some(cached) = cache.tracked_get(|cache| &mut cache.dynamic_field_type_cache, member_id)
    {
        return cached;
    }

    if let Some(mark) = cache.dynamic_field_resolving.get(member_id) {
        crate::db_index::derivation::consult_cycle(*mark);
        return None;
    }

    let frame = crate::db_index::derivation::open_frame();
    cache
        .dynamic_field_resolving
        .insert(*member_id, crate::db_index::derivation::scope_mark());
    let result = dynamic_field_member_type_inner(db, cache, member_id);
    cache.dynamic_field_resolving.remove(member_id);
    cache.tracked_insert(
        |cache| &mut cache.dynamic_field_type_cache,
        *member_id,
        result.clone(),
        frame,
    );
    result
}

fn dynamic_field_member_type_inner(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    member_id: &LuaMemberId,
) -> Option<LuaType> {
    if let Some(type_cache) = db.get_type_index().get_type_cache(&(*member_id).into()) {
        let typ = type_cache.as_type().clone();
        if !matches!(
            typ,
            LuaType::Any | LuaType::Unknown | LuaType::Nil | LuaType::Table
        ) {
            return Some(typ);
        }
    }

    let root = db
        .get_vfs()
        .syntax_tree_around(&member_id.file_id, member_id.get_syntax_id().get_range())?
        .get_red_root();
    let node = member_id.get_syntax_id().to_node_from_root(&root)?;

    if let Some(table_field) = LuaTableField::cast(node.clone()) {
        let value_expr = table_field.get_value_expr()?;
        let mut definition_cache = dynamic_field_definition_cache(cache, member_id.file_id);
        return infer_expr(db, &mut definition_cache, value_expr).ok();
    }

    if let Some(index_expr) = LuaIndexExpr::cast(node.clone()) {
        let assign_node = index_expr.syntax().parent()?;
        let assign_stat = LuaAssignStat::cast(assign_node)?;
        let (vars, exprs) = assign_stat.get_var_and_expr_list();
        let value_idx = vars
            .iter()
            .position(|var| var.syntax().text_range() == node.text_range())?;
        let mut definition_cache = dynamic_field_definition_cache(cache, member_id.file_id);
        return infer_expr_list_value_type_at(db, &mut definition_cache, &exprs, value_idx)
            .ok()
            .map(|typ| typ.unwrap_or(LuaType::Nil));
    }

    None
}

fn dynamic_field_definition_cache(cache: &LuaInferCache, file_id: FileId) -> LuaInferCache {
    let mut definition_cache = LuaInferCache::new(file_id, cache.get_config().clone());
    definition_cache.dynamic_field_type_cache = cache.dynamic_field_type_cache.clone();
    definition_cache.dynamic_field_resolving = cache.dynamic_field_resolving.clone();
    definition_cache
}

fn dynamic_field_definitions(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    caller_file_id: FileId,
    prefix_type: &LuaType,
    field_name: &str,
    access_position: Option<TextSize>,
) -> Vec<VisibleDynamicFieldDefinition> {
    match prefix_type {
        LuaType::Ref(type_id) | LuaType::Def(type_id) => dynamic_field_definitions_for_owner(
            db,
            cache,
            caller_file_id,
            &crate::DynamicFieldOwner::Type(type_id.clone()),
            field_name,
            access_position,
            None,
        ),
        LuaType::TableConst(table_range) => {
            let table_owner = crate::DynamicFieldOwner::Table(table_range.clone());
            let owner = crate::canonical_dynamic_field_owner(db, table_owner.clone());
            let source_min_position = (owner != table_owner)
                .then(|| InFiled::new(table_range.file_id, table_range.value.start()));
            dynamic_field_definitions_for_owner(
                db,
                cache,
                caller_file_id,
                &owner,
                field_name,
                access_position,
                source_min_position.as_ref(),
            )
        }
        LuaType::Instance(instance) => dynamic_field_definitions(
            db,
            cache,
            caller_file_id,
            instance.get_base(),
            field_name,
            access_position,
        ),
        _ => Vec::new(),
    }
}
/// Reads `owner`'s `key` slot and its dynamic field, which register a lookup
/// as their reader; the pass collecting dynamic fields never sees them, so it
/// reads only the slot.
pub(crate) fn record_member_slot_read(db: &DbIndex, owner: &LuaMemberOwner, key: &LuaMemberKey) {
    db.get_member_index()
        .get_member_count_for_owner_key(owner, key);
    if let LuaMemberKey::Name(name) = key
        && crate::db_index::derivation::dynamic_fields_visible()
        && let Some(dynamic_owner) = crate::dynamic_field_owner_of(db, owner)
    {
        db.get_dynamic_field_index()
            .field_definitions(&dynamic_owner, name.as_str());
    }
}

/// Records the masked `(owner, field)` read a visible pass re-derives from;
/// the pass collecting dynamic fields never sees them, so it records none.
fn record_dynamic_field_read_for_prefix(db: &DbIndex, prefix_type: &LuaType, field_name: &str) {
    if !crate::db_index::derivation::dynamic_fields_visible() {
        return;
    }
    // `field_definitions` records the `(owner, field)` read itself.
    fn flag_masked_if_present(db: &DbIndex, owner: &crate::DynamicFieldOwner, field_name: &str) {
        db.get_dynamic_field_index()
            .field_definitions(owner, field_name);
    }

    match prefix_type {
        LuaType::Ref(type_id) | LuaType::Def(type_id) => {
            flag_masked_if_present(
                db,
                &crate::DynamicFieldOwner::Type(type_id.clone()),
                field_name,
            );
        }
        LuaType::TableConst(table_range) => {
            let owner = crate::canonical_dynamic_field_owner(
                db,
                crate::DynamicFieldOwner::Table(table_range.clone()),
            );
            flag_masked_if_present(db, &owner, field_name);
        }
        LuaType::Instance(instance) => {
            record_dynamic_field_read_for_prefix(db, instance.get_base(), field_name);
        }
        _ => {}
    }
}

fn dynamic_field_definitions_for_owner(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    caller_file_id: FileId,
    owner: &crate::DynamicFieldOwner,
    field_name: &str,
    access_position: Option<TextSize>,
    source_min_position: Option<&InFiled<TextSize>>,
) -> Vec<VisibleDynamicFieldDefinition> {
    let dynamic_fields_global = db.get_emmyrc().gmod.dynamic_fields_global;
    let caller_mask = effective_dynamic_field_state_mask(db, caller_file_id, access_position);
    let access_function = access_position.and_then(|position| {
        db.get_member_index()
            .enclosing_function_scope_range(caller_file_id, position)
    });
    let mut visible = Vec::new();
    // Consulted in the index's canonical order, unfiltered and unsorted: the
    // elected arm and the elected semantic decl both follow it.
    for definition in db
        .get_dynamic_field_index()
        .field_definitions(owner, field_name)
    {
        if source_min_position.is_some_and(|source_min_position| {
            !source_survives_table_generation_cutoff(
                db,
                definition.file_id,
                definition.value.start(),
                source_min_position,
            )
        }) {
            continue;
        }
        if !(dynamic_fields_global || definition.file_id == caller_file_id) {
            continue;
        }
        if !is_dynamic_field_realm_compatible(db, caller_mask, definition) {
            continue;
        }
        let syntax = dynamic_field_definition_syntax(db, cache, definition);
        if let Some(visibility) = dynamic_field_definition_visibility_at(
            db,
            caller_file_id,
            definition,
            syntax.enclosing_assign_range,
            access_position,
            access_function,
        ) {
            visible.push(VisibleDynamicFieldDefinition {
                member_id: syntax.member_id,
                visibility,
            });
        }
    }
    visible
}

fn dynamic_field_definition_visibility_at(
    db: &DbIndex,
    caller_file_id: FileId,
    definition: &crate::InFiled<TextRange>,
    enclosing_assign_range: Option<TextRange>,
    access_position: Option<TextSize>,
    access_function: Option<TextRange>,
) -> Option<DynamicFieldDefinitionVisibility> {
    let Some(access_position) = access_position else {
        return Some(DynamicFieldDefinitionVisibility::Runtime);
    };
    if definition.file_id != caller_file_id {
        return Some(DynamicFieldDefinitionVisibility::Runtime);
    }
    if enclosing_assign_range
        .is_some_and(|range| range.contains(access_position) && range != definition.value)
    {
        return None;
    }

    let member_index = db.get_member_index();
    let definition_function =
        member_index.enclosing_function_scope_range(definition.file_id, definition.value.start());

    if definition_function != access_function
        && (definition_function.is_some() || access_function.is_some())
    {
        // A write in a function that encloses the reading one runs before the
        // inner function can be called, so it is at least as good evidence as
        // the cross-file write trusted unconditionally above.
        if let (Some(definition_function), Some(access_function)) =
            (definition_function, access_function)
            && definition_function.contains_range(access_function)
            && definition.value.start() <= access_position
        {
            return Some(DynamicFieldDefinitionVisibility::Runtime);
        }
        return Some(DynamicFieldDefinitionVisibility::ShapeOnly);
    }

    (definition.value.start() <= access_position)
        .then_some(DynamicFieldDefinitionVisibility::Runtime)
}

/// The definition site's member id and enclosing assignment range.
///
/// Both are read off the definition file's tree alone, so they are memoised
/// per definition rather than re-walked from the root for every access
/// position that consults the same definition.
fn dynamic_field_definition_syntax(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    definition: &crate::InFiled<TextRange>,
) -> DynamicFieldDefinitionSyntax {
    if let Some(cached) = cache.tracked_get(
        |cache| &mut cache.dynamic_field_definition_syntax_cache,
        definition,
    ) {
        return cached;
    }
    let frame = crate::db_index::derivation::open_frame();
    let syntax = dynamic_field_definition_syntax_uncached(db, definition);
    cache.tracked_insert(
        |cache| &mut cache.dynamic_field_definition_syntax_cache,
        definition.clone(),
        syntax,
        frame,
    );
    syntax
}

fn dynamic_field_definition_syntax_uncached(
    db: &DbIndex,
    definition: &crate::InFiled<TextRange>,
) -> DynamicFieldDefinitionSyntax {
    let mut syntax = DynamicFieldDefinitionSyntax::default();
    let file_id = definition.file_id;
    let range = definition.value;
    let Some(tree) = db.get_vfs().syntax_tree_at_statement(&file_id, range) else {
        return syntax;
    };
    let root = tree.get_red_root();
    let Some(token) = root.token_at_offset(range.start()).right_biased() else {
        return syntax;
    };

    let mut current = token.parent();
    while let Some(node) = current {
        if syntax.member_id.is_none() {
            if let Some(index_expr) = LuaIndexExpr::cast(node.clone()) {
                // Legacy: range matches the full index expression. New: range
                // matches the index key (field name) within it.
                if index_expr.get_range() == range
                    || index_expr.get_index_key().and_then(|key| key.get_range()) == Some(range)
                {
                    syntax.member_id = Some(LuaMemberId::new(index_expr.get_syntax_id(), file_id));
                }
            }
            if syntax.member_id.is_none()
                && let Some(table_field) = LuaTableField::cast(node.clone())
                && (table_field.get_range() == range
                    || table_field.get_field_key().and_then(|key| key.get_range()) == Some(range))
            {
                syntax.member_id = Some(LuaMemberId::new(table_field.get_syntax_id(), file_id));
            }
        }
        if syntax.enclosing_assign_range.is_none()
            && let Some(assign_stat) = LuaAssignStat::cast(node.clone())
        {
            syntax.enclosing_assign_range = Some(assign_stat.get_range());
        }
        if syntax.member_id.is_some() && syntax.enclosing_assign_range.is_some() {
            break;
        }
        current = node.parent();
    }

    syntax
}

fn effective_dynamic_field_state_mask(
    db: &DbIndex,
    file_id: FileId,
    position: Option<TextSize>,
) -> GmodStateMask {
    let infer_index = db.get_gmod_infer_index();
    let Some(position) = position else {
        return infer_index
            .get_realm_file_metadata(&file_id)
            .map_or(GmodStateMask::empty(), |metadata| {
                metadata.inferred_realm.state_mask()
            });
    };

    infer_index
        .get_member_annotation_realm_at_offset(&file_id, position)
        .map(crate::GmodRealm::state_mask)
        .unwrap_or_else(|| infer_index.get_state_mask_at_offset(&file_id, position))
}

fn is_dynamic_field_realm_compatible(
    db: &DbIndex,
    caller_mask: GmodStateMask,
    definition: &crate::InFiled<TextRange>,
) -> bool {
    if !db.get_emmyrc().gmod.enabled {
        return true;
    }

    caller_mask.is_compatible_with(effective_dynamic_field_state_mask(
        db,
        definition.file_id,
        Some(definition.value.start()),
    ))
}

pub(crate) fn member_key_as_type(key: &LuaMemberKey) -> Option<LuaType> {
    match key {
        LuaMemberKey::None => None,
        LuaMemberKey::Integer(i) => Some(LuaType::IntegerConst(*i)),
        LuaMemberKey::Name(name) => Some(LuaType::StringConst(name.clone().into())),
        LuaMemberKey::ExprType(typ) => Some(typ.clone()),
    }
}

pub fn get_member_value_expr(db: &DbIndex, member_id: LuaMemberId) -> Option<LuaExpr> {
    let root = db
        .get_vfs()
        .syntax_tree_around(&member_id.file_id, member_id.get_syntax_id().get_range())?
        .get_red_root();
    let node = member_id.get_syntax_id().to_node_from_root(&root)?;

    if let Some(field) = LuaTableField::cast(node.clone()) {
        return field.get_value_expr();
    }

    if let Some(index_expr) = LuaIndexExpr::cast(node) {
        if let Some(assign_stat) = index_expr.get_parent::<LuaAssignStat>() {
            let (vars, value_exprs) = assign_stat.get_var_and_expr_list();
            let value_idx = vars
                .iter()
                .position(|var| var.get_syntax_id() == index_expr.get_syntax_id())?;
            return value_exprs.get(value_idx).cloned();
        }

        if let Some(func_stat) = index_expr.get_parent::<LuaFuncStat>() {
            return func_stat.get_closure().map(LuaExpr::ClosureExpr);
        }
    }

    None
}

pub(crate) fn member_key_matches_type(
    db: &DbIndex,
    access_key_type: &LuaType,
    member_key: &LuaMemberKey,
) -> bool {
    AccessKeyMatcher::new(access_key_type).matches(db, member_key)
}

/// An access key prepared once for matching against many member keys.
pub(crate) struct AccessKeyMatcher<'a> {
    access_key_type: &'a LuaType,
    literal_keys: Option<FxHashSet<LuaMemberKey>>,
}

impl<'a> AccessKeyMatcher<'a> {
    pub(crate) fn new(access_key_type: &'a LuaType) -> Self {
        let literal_keys = match access_key_type {
            LuaType::Union(union) => union.types().map(literal_type_member_key).collect(),
            _ => None,
        };
        Self {
            access_key_type,
            literal_keys,
        }
    }

    pub(crate) fn matches(&self, db: &DbIndex, member_key: &LuaMemberKey) -> bool {
        if let LuaMemberKey::ExprType(member_key_type) = member_key
            && member_key_type.is_unknown()
        {
            return unknown_index_key_matches_access(self.access_key_type);
        }

        if let Some(literal_keys) = &self.literal_keys {
            match member_key {
                LuaMemberKey::Name(_) | LuaMemberKey::Integer(_) => {
                    return literal_keys.contains(member_key);
                }
                LuaMemberKey::ExprType(typ) => {
                    if let Some(key) = literal_type_member_key(typ) {
                        return literal_keys.contains(&key);
                    }
                }
                LuaMemberKey::None => return false,
            }
        }

        let Some(member_key_type) = member_key_as_type(member_key) else {
            return false;
        };
        member_key_types_overlap(db, self.access_key_type, &member_key_type)
    }
}

fn literal_type_member_key(typ: &LuaType) -> Option<LuaMemberKey> {
    match typ {
        LuaType::StringConst(name) | LuaType::DocStringConst(name) => {
            Some(LuaMemberKey::Name((**name).clone()))
        }
        LuaType::IntegerConst(value) | LuaType::DocIntegerConst(value) => {
            Some(LuaMemberKey::Integer(*value))
        }
        _ => None,
    }
}

fn member_key_types_overlap(db: &DbIndex, access: &LuaType, member: &LuaType) -> bool {
    match (access, member) {
        (LuaType::Union(union), _) => union
            .types()
            .any(|arm| member_key_types_overlap(db, arm, member)),
        (_, LuaType::Union(union)) => union
            .types()
            .any(|arm| member_key_types_overlap(db, access, arm)),
        (LuaType::TypeGuard(inner), _) => member_key_types_overlap(db, inner, member),
        (_, LuaType::TypeGuard(inner)) => member_key_types_overlap(db, access, inner),
        _ => literal_member_key_match(access, member).unwrap_or_else(|| {
            check_type_compact(db, access, member).is_ok()
                || check_type_compact(db, member, access).is_ok()
        }),
    }
}

/// Whether an access typed `access_key_type` reaches a member keyed by
/// `member_key_type` when a literal decides it: a literal matches only its
/// equal, and a primitive access reaches every literal key of its kind.
fn literal_member_key_match(access_key_type: &LuaType, member_key_type: &LuaType) -> Option<bool> {
    match (access_key_type, member_key_type) {
        (LuaType::StringConst(left), LuaType::StringConst(right))
        | (LuaType::StringConst(left), LuaType::DocStringConst(right))
        | (LuaType::DocStringConst(left), LuaType::StringConst(right))
        | (LuaType::DocStringConst(left), LuaType::DocStringConst(right)) => Some(left == right),
        (LuaType::IntegerConst(left), LuaType::IntegerConst(right))
        | (LuaType::IntegerConst(left), LuaType::DocIntegerConst(right))
        | (LuaType::DocIntegerConst(left), LuaType::IntegerConst(right))
        | (LuaType::DocIntegerConst(left), LuaType::DocIntegerConst(right)) => Some(left == right),
        (LuaType::BooleanConst(left), LuaType::BooleanConst(right))
        | (LuaType::BooleanConst(left), LuaType::DocBooleanConst(right))
        | (LuaType::DocBooleanConst(left), LuaType::BooleanConst(right))
        | (LuaType::DocBooleanConst(left), LuaType::DocBooleanConst(right)) => Some(left == right),
        (LuaType::Any | LuaType::Unknown, LuaType::StringConst(_) | LuaType::IntegerConst(_))
        | (LuaType::Integer | LuaType::Number, LuaType::IntegerConst(_))
        | (LuaType::String, LuaType::StringConst(_)) => Some(true),
        _ => None,
    }
}

fn unknown_index_key_matches_access(access_key_type: &LuaType) -> bool {
    match access_key_type {
        LuaType::Any | LuaType::Unknown | LuaType::String | LuaType::Number | LuaType::Integer => {
            true
        }
        LuaType::TypeGuard(inner) => unknown_index_key_matches_access(inner),
        LuaType::Union(union) => union.types().any(unknown_index_key_matches_access),
        _ => false,
    }
}

pub fn find_member_origin_owner(
    db: &DbIndex,
    infer_config: &mut LuaInferCache,
    member_id: LuaMemberId,
) -> Option<LuaSemanticDeclId> {
    find_member_origin_owner_inner(db, infer_config, member_id, None)
}

pub fn find_member_origin_owner_at_offset(
    db: &DbIndex,
    infer_config: &mut LuaInferCache,
    member_id: LuaMemberId,
    caller_position: rowan::TextSize,
) -> Option<LuaSemanticDeclId> {
    find_member_origin_owner_inner(db, infer_config, member_id, Some(caller_position))
}

fn find_member_origin_owner_inner(
    db: &DbIndex,
    infer_config: &mut LuaInferCache,
    member_id: LuaMemberId,
    caller_position: Option<rowan::TextSize>,
) -> Option<LuaSemanticDeclId> {
    let mut visited_members = FxHashSet::default();

    let mut current_owner = resolve_member_owner(db, infer_config, &member_id, caller_position);
    let mut final_owner = current_owner.clone();

    while let Some(LuaSemanticDeclId::Member(current_member_id)) = &current_owner {
        if visited_members.contains(current_member_id) {
            break;
        }

        visited_members.insert(*current_member_id);

        match resolve_member_owner(db, infer_config, current_member_id, caller_position) {
            Some(next_owner) => {
                final_owner = Some(next_owner.clone());
                current_owner = Some(next_owner);
            }
            None => break,
        }
    }

    final_owner
}

fn resolve_member_owner(
    db: &DbIndex,
    infer_config: &mut LuaInferCache,
    member_id: &LuaMemberId,
    caller_position: Option<rowan::TextSize>,
) -> Option<LuaSemanticDeclId> {
    let root = db
        .get_vfs()
        .syntax_tree_around(&member_id.file_id, member_id.get_syntax_id().get_range())?
        .get_red_root();
    let current_node = member_id.get_syntax_id().to_node_from_root(&root)?;
    match member_id.get_syntax_id().get_kind() {
        LuaSyntaxKind::TableFieldAssign => {
            if LuaTableField::can_cast(current_node.kind().into()) {
                let table_field = LuaTableField::cast(current_node.clone())?;
                // If the table is a class, get the owner through type inference
                if let Some(owner_id) = resolve_table_field_through_type_inference(
                    db,
                    infer_config,
                    &table_field,
                    caller_position,
                ) {
                    return Some(owner_id);
                }
                // Not a class, so infer through the right value
                let value_expr = table_field.get_value_expr()?;
                let value_node = value_expr.get_syntax_id().to_node_from_root(&root)?;
                infer_node_semantic_decl(db, infer_config, value_node, SemanticDeclLevel::default())
            } else {
                None
            }
        }
        LuaSyntaxKind::IndexExpr => {
            let assign_node = current_node.parent()?;
            let assign_stat = LuaAssignStat::cast(assign_node)?;
            let (vars, exprs) = assign_stat.get_var_and_expr_list();

            for (var, expr) in vars.iter().zip(exprs.iter()) {
                if var.syntax().text_range() == current_node.text_range() {
                    let expr_node = expr.get_syntax_id().to_node_from_root(&root)?;
                    return infer_node_semantic_decl(
                        db,
                        infer_config,
                        expr_node,
                        SemanticDeclLevel::default(),
                    );
                }
            }
            None
        }
        _ => None,
    }
}

fn resolve_table_field_through_type_inference(
    db: &DbIndex,
    infer_config: &mut LuaInferCache,
    table_field: &LuaTableField,
    caller_position: Option<rowan::TextSize>,
) -> Option<LuaSemanticDeclId> {
    let parent = table_field.syntax().parent()?;
    let table_expr = LuaTableExpr::cast(parent)?;
    let table_type = infer_table_should_be(db, infer_config, table_expr).ok()?;

    if !matches!(table_type, LuaType::Ref(_) | LuaType::Def(_)) {
        return None;
    }

    let field_key = table_field.get_field_key()?;
    let key = LuaMemberKey::from_index_key(db, infer_config, &field_key).ok()?;
    let caller_file_id = infer_config.get_file_id();
    let workspace_id = db.get_module_index().get_workspace_id(caller_file_id);
    let member_infos = match (workspace_id, caller_position) {
        (Some(workspace_id), Some(caller_position)) => {
            find_members_with_key_in_workspace_for_file_at_offset(
                db,
                &table_type,
                key,
                false,
                workspace_id,
                caller_file_id,
                caller_position,
            )?
        }
        (Some(workspace_id), None) => find_members_with_key_in_workspace_for_file(
            db,
            &table_type,
            key,
            false,
            workspace_id,
            caller_file_id,
        )?,
        (None, _) => find_members_with_key(db, &table_type, key, false)?,
    };

    member_infos
        .first()
        .cloned()
        .and_then(|m| m.property_owner_id)
}
