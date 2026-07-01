use crate::{DbIndex, GlobalId, LuaDeclId, LuaMemberId, LuaMemberOwner, LuaTypeOwner};

use super::get_owner_id;

pub fn migrate_global_members_when_type_resolve(
    db: &mut DbIndex,
    type_owner: LuaTypeOwner,
) -> Option<()> {
    match type_owner {
        LuaTypeOwner::Decl(decl_id) => {
            migrate_global_member_to_decl(db, decl_id);
        }
        LuaTypeOwner::Member(member_id) => {
            migrate_global_member_to_member(db, member_id);
        }
        _ => {}
    }
    Some(())
}

pub fn migrate_global_path_members_when_owner_resolved(
    db: &mut DbIndex,
    global_id: &GlobalId,
) -> Option<()> {
    let decl_ids = db
        .get_global_index()
        .get_global_decl_ids(global_id.get_name())?
        .clone();

    for decl_id in decl_ids {
        alias_global_members_to_decl_owner(db, decl_id);
    }

    Some(())
}

fn alias_global_members_to_decl_owner(db: &mut DbIndex, decl_id: LuaDeclId) -> Option<()> {
    let decl = db.get_decl_index().get_decl(&decl_id)?;
    if !decl.is_global() {
        return None;
    }

    let owner_id = get_owner_id(db, &decl_id.into())?;

    let name = decl.get_name();
    let global_id = GlobalId::new(name);
    let members = db
        .get_member_index()
        .get_members(&LuaMemberOwner::GlobalPath(global_id))?
        .iter()
        .filter(|member| member.get_feature().is_meta_decl())
        .map(|member| member.get_id())
        .collect::<Vec<_>>();

    let member_index = db.get_member_index_mut();
    for member_id in members {
        member_index.add_member_alias_to_owner(owner_id.clone(), member_id);
    }

    Some(())
}

fn migrate_global_member_to_decl(db: &mut DbIndex, decl_id: LuaDeclId) -> Option<()> {
    let decl = db.get_decl_index().get_decl(&decl_id)?;
    if !decl.is_global() {
        return None;
    }

    let owner_id = get_owner_id(db, &decl_id.into())?;

    let name = decl.get_name();
    let global_id = GlobalId::new(name);
    let members = db
        .get_member_index()
        .get_members(&LuaMemberOwner::GlobalPath(global_id))?
        .iter()
        .map(|member| member.get_id())
        .collect::<Vec<_>>();

    let member_index = db.get_member_index_mut();
    for member_id in members {
        member_index.set_member_owner(owner_id.clone(), member_id.file_id, member_id);
        member_index.add_member_to_owner(owner_id.clone(), member_id);
    }

    Some(())
}

fn migrate_global_member_to_member(db: &mut DbIndex, member_id: LuaMemberId) -> Option<()> {
    let member = db.get_member_index().get_member(&member_id)?;
    let global_id = member.get_global_id()?;
    let owner_id = get_owner_id(db, &member_id.into())?;

    let members = db
        .get_member_index()
        .get_members(&LuaMemberOwner::GlobalPath(global_id.clone()))?
        .iter()
        .map(|member| member.get_id())
        .collect::<Vec<_>>();

    let member_index = db.get_member_index_mut();
    for member_id in members {
        member_index.set_member_owner(owner_id.clone(), member_id.file_id, member_id);
        member_index.add_member_to_owner(owner_id.clone(), member_id);
    }

    Some(())
}

#[cfg(test)]
mod tests {
    use glua_parser::{LuaSyntaxId, LuaSyntaxKind};
    use rowan::{TextRange, TextSize};

    use crate::{
        FileId, GlobalId, LuaDecl, LuaDeclExtra, LuaDeclarationTree, LuaMember, LuaMemberFeature,
        LuaMemberKey, LuaMemberOwner, LuaType, LuaTypeCache, LuaTypeDeclId, LuaTypeOwner,
    };

    use super::*;

    fn syntax_id(kind: LuaSyntaxKind, start: u32) -> LuaSyntaxId {
        LuaSyntaxId::new(
            kind.into(),
            TextRange::new(TextSize::new(start), TextSize::new(start + 1)),
        )
    }

    #[test]
    fn alias_global_members_to_decl_owner_only_aliases_meta_members() {
        let mut db = DbIndex::new();
        let decl_file = FileId::new(1);
        let decl = LuaDecl::new(
            "math",
            decl_file,
            TextRange::new(TextSize::new(0), TextSize::new(4)),
            LuaDeclExtra::Global {
                kind: LuaSyntaxKind::NameExpr.into(),
            },
            None,
        );
        let decl_id = decl.get_id();
        let mut decl_tree = LuaDeclarationTree::new(decl_file);
        decl_tree.add_decl(decl);
        db.get_decl_index_mut().add_decl_tree(decl_tree);
        db.get_global_index_mut().add_global_decl("math", decl_id);

        let math_type_id = LuaTypeDeclId::global("mathlib");
        db.get_type_index_mut().bind_type(
            LuaTypeOwner::Decl(decl_id),
            LuaTypeCache::DocType(LuaType::Ref(math_type_id.clone())),
        );

        let global_owner = LuaMemberOwner::GlobalPath(GlobalId::new("math"));
        let meta_member_id =
            LuaMemberId::new(syntax_id(LuaSyntaxKind::IndexExpr, 10), FileId::new(2));
        let file_member_id =
            LuaMemberId::new(syntax_id(LuaSyntaxKind::IndexExpr, 20), FileId::new(3));
        db.get_member_index_mut().add_member(
            global_owner.clone(),
            LuaMember::new(
                meta_member_id,
                LuaMemberKey::Name("Clamp".into()),
                LuaMemberFeature::MetaMethodDecl,
                Some(GlobalId::new("math.Clamp")),
            ),
        );
        db.get_member_index_mut().add_member(
            global_owner,
            LuaMember::new(
                file_member_id,
                LuaMemberKey::Name("AddonOnly".into()),
                LuaMemberFeature::FileMethodDecl,
                Some(GlobalId::new("math.AddonOnly")),
            ),
        );

        alias_global_members_to_decl_owner(&mut db, decl_id);

        let resolved_owner = LuaMemberOwner::Type(math_type_id);
        let member_index = db.get_member_index();
        assert!(
            member_index
                .get_member_item(&resolved_owner, &LuaMemberKey::Name("Clamp".into()))
                .is_some(),
            "meta global-path members should be visible on the resolved global owner"
        );
        assert!(
            member_index
                .get_member_item(&resolved_owner, &LuaMemberKey::Name("AddonOnly".into()))
                .is_none(),
            "non-meta global-path members should not be aliased by the late meta bridge"
        );
    }
}
