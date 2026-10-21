//! The gamemode hook owners: the globals whose methods are hooks, and the
//! owners whose hooks stand in for each one's, for docs and completion.

use std::collections::HashSet;

use crate::{DbIndex, LuaType, LuaTypeDeclId};

/// The globals whose methods are gamemode hooks with no config.
pub const BUILTIN_HOOK_OWNERS: [&str; 4] = ["GM", "GAMEMODE", "SANDBOX", "PLUGIN"];

/// The hook owners sharing `owner_name`'s hooks, itself among them; empty
/// when it is no hook owner.
pub fn gmod_hook_owner_group(owner_name: &str) -> &'static [&'static str] {
    if owner_name.eq_ignore_ascii_case("GM") || owner_name.eq_ignore_ascii_case("GAMEMODE") {
        &["GM", "GAMEMODE", "SANDBOX"]
    } else if owner_name.eq_ignore_ascii_case("PLUGIN") {
        &["PLUGIN", "GM", "GAMEMODE", "SANDBOX"]
    } else if owner_name.eq_ignore_ascii_case("SANDBOX") {
        &["SANDBOX", "GM", "GAMEMODE"]
    } else {
        &[]
    }
}

/// The hook-owner group of `owner`: that of the nearest of `owner` and its
/// supertypes that is a hook owner, so a gamemode class based on `GM` shares
/// the hooks `GM` does.
pub fn gmod_hook_owner_group_of(db: &DbIndex, owner: &LuaTypeDeclId) -> &'static [&'static str] {
    let mut pending = vec![owner.clone()];
    let mut visited = HashSet::new();
    while let Some(type_id) = pending.pop() {
        if !visited.insert(type_id.clone()) {
            continue;
        }
        let group = gmod_hook_owner_group(type_id.get_simple_name());
        if !group.is_empty() {
            return group;
        }
        for super_type in db
            .get_type_index()
            .get_super_types(&type_id)
            .unwrap_or_default()
        {
            if let LuaType::Ref(super_id) | LuaType::Def(super_id) = super_type {
                pending.push(super_id);
            }
        }
    }
    &[]
}
