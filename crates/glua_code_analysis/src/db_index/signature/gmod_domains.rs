//! Centralized Garry's Mod metadata domains and roles.
//!
//! This module provides typed/shared constants for the GMod metadata domains
//! currently consumed by call-arg roles (`@call_arg`, `@call_arg_field`,
//! `@overload_call_arg`, `@overload_call_arg_field`), plus a small set of
//! Phase 1 metadata names reserved for future annotation-driven recognizer
//! phases (`gmod.member_guard`, `gmod.self_guard`, `gmod.net_payload`).
//!
//! It also exposes cheap, general-purpose helpers built on top of the existing
//! `LuaSignature::visit_call_arg_roles_for_param` /
//! `find_call_arg_role_from_type` infrastructure and the existing
//! `LuaPropertyIndex` standalone-attribute storage keyed by
//! `LuaSemanticDeclId::Signature`. No class/type-level `---@class` attribute
//! support is added here; signature-level standalone attributes only.
//!
//! These helpers are intentionally mechanical: they centralize raw domain/role
//! strings and selection logic so analyzer and LSP code can consume them
//! without duplicating literals, and without introducing broad per-call
//! semantic inference or whole-workspace scans.

use crate::{DbIndex, LuaSignatureId};

use super::signature::{LuaCallArgRole, LuaSignature, visit_call_arg_roles_from_type};

// ---------------------------------------------------------------------------
// Domain constants.
// ---------------------------------------------------------------------------

/// Load/path discovery domain (e.g. `include`, `AddCSLuaFile`, `require`).
pub const GMOD_DOMAIN_LOAD: &str = "gmod.load";

/// Document-color domain (e.g. `Color(r, g, b, a)` channel roles).
pub const GMOD_DOMAIN_COLOR: &str = "gmod.color";

/// Scripted-class base reference domain (e.g. `DEFINE_BASECLASS`).
pub const GMOD_DOMAIN_CLASS_BASE: &str = "gmod.class_base";

/// Gamemode derivation domain (e.g. `DeriveGamemode`).
pub const GMOD_DOMAIN_GAMEMODE: &str = "gmod.gamemode";

/// Network variable definition domain (e.g. `Entity:NetworkVar`).
pub const GMOD_DOMAIN_NETWORK_VAR: &str = "gmod.network_var";

/// VGUI panel definition/registration domain.
pub const GMOD_DOMAIN_VGUI_PANEL: &str = "gmod.vgui_panel";

/// Derma skin definition/reference domain.
pub const GMOD_DOMAIN_DERMA_SKIN: &str = "gmod.derma_skin";

/// Net message definition/start/receive domain.
pub const GMOD_DOMAIN_NET_MESSAGE: &str = "gmod.net_message";

/// Hook registration/emission/removal domain.
pub const GMOD_DOMAIN_HOOK: &str = "gmod.hook";

/// Concommand definition/callback domain.
pub const GMOD_DOMAIN_CONCOMMAND: &str = "gmod.concommand";

/// ConVar definition domain (server/client).
pub const GMOD_DOMAIN_CONVAR: &str = "gmod.convar";

/// Timer definition/callback domain.
pub const GMOD_DOMAIN_TIMER: &str = "gmod.timer";

/// File discovery (`file.Find`) domain.
pub const GMOD_DOMAIN_FILE_FIND: &str = "gmod.file_find";

// ---------------------------------------------------------------------------
// Phase 1 reserved metadata names.
// ---------------------------------------------------------------------------

/// Reserved for future member-guard call-argument roles: a guard predicate
/// parameter that intentionally accepts an index/member expression whose
/// existence or callable shape is being tested.
pub const GMOD_DOMAIN_MEMBER_GUARD: &str = "gmod.member_guard";

/// Reserved for future self-guard markers: a signature that establishes a
/// guarded `self` parameter contract. Modeled as a signature-level standalone
/// attribute.
pub const GMOD_DOMAIN_SELF_GUARD: &str = "gmod.self_guard";

/// Reserved for future net-payload markers: a signature carrying or consuming a
/// typed net payload. Modeled as a signature-level standalone attribute.
pub const GMOD_DOMAIN_NET_PAYLOAD: &str = "gmod.net_payload";

/// All currently-active GMod call-arg domains, sorted for stable, deterministic
/// iteration. Callers must never assume any domain-specific precedence from the
/// order here; role priority comes from each role's `priority` field.
pub const GMOD_CALL_ARG_DOMAINS: &[&str] = &[
    GMOD_DOMAIN_CLASS_BASE,
    GMOD_DOMAIN_COLOR,
    GMOD_DOMAIN_CONCOMMAND,
    GMOD_DOMAIN_CONVAR,
    GMOD_DOMAIN_DERMA_SKIN,
    GMOD_DOMAIN_FILE_FIND,
    GMOD_DOMAIN_GAMEMODE,
    GMOD_DOMAIN_HOOK,
    GMOD_DOMAIN_LOAD,
    GMOD_DOMAIN_NET_MESSAGE,
    GMOD_DOMAIN_NETWORK_VAR,
    GMOD_DOMAIN_TIMER,
    GMOD_DOMAIN_VGUI_PANEL,
];

/// Phase 1 reserved signature-level metadata domains (no call-arg roles yet).
pub const GMOD_SIGNATURE_METADATA_DOMAINS: &[&str] =
    &[GMOD_DOMAIN_SELF_GUARD, GMOD_DOMAIN_NET_PAYLOAD];

// ---------------------------------------------------------------------------
// Call-arg role selection helpers.
// ---------------------------------------------------------------------------

/// Returns the highest-priority call-arg role attached to `param_idx` of
/// `signature` whose `domain` matches and whose `role` is listed in `roles`.
///
/// This is a thin, cheap selector over [`LuaSignature::visit_call_arg_roles_for_param`]:
/// it visits only the roles already attached to the given signature/param (plus
/// its overloads) and picks the one with the largest `priority` (missing
/// priorities are treated as `0`). It performs no per-call semantic inference
/// and does not scan other signatures.
///
/// Pass `&[]` for `roles` to match any role within `domain`.
pub fn find_best_call_arg_role_for_param(
    signature: &LuaSignature,
    param_idx: usize,
    domain: &str,
    roles: &[&str],
) -> Option<LuaCallArgRole> {
    let mut best: Option<LuaCallArgRole> = None;
    let mut consider = |role: &LuaCallArgRole| {
        if role.domain != domain {
            return;
        }
        if !roles.is_empty() && !roles.iter().any(|candidate| *candidate == role.role) {
            return;
        }
        if best
            .as_ref()
            .is_none_or(|current| role.priority.unwrap_or(0) > current.priority.unwrap_or(0))
        {
            best = Some(role.clone());
        }
    };
    signature.visit_call_arg_roles_for_param(param_idx, &mut consider);
    best
}

/// Like [`find_best_call_arg_role_for_param`] but resolves roles from a
/// `LuaType` (typically a callee's resolved type) via the existing
/// [`visit_call_arg_roles_from_type`] traversal. Useful for callers that
/// already hold the resolved callee type rather than a `LuaSignature` handle.
///
/// Pass `&[]` for `roles` to match any role within `domain`, matching
/// [`find_best_call_arg_role_for_param`].
pub fn find_best_call_arg_role_from_type(
    db: &DbIndex,
    typ: &crate::LuaType,
    arg_idx: usize,
    domain: &str,
    roles: &[&str],
) -> Option<LuaCallArgRole> {
    let mut best: Option<LuaCallArgRole> = None;
    visit_call_arg_roles_from_type(db, typ, arg_idx, &mut |role| {
        if role.domain != domain {
            return;
        }
        if !roles.is_empty() && !roles.iter().any(|candidate| *candidate == role.role) {
            return;
        }
        if best
            .as_ref()
            .is_none_or(|current| role.priority.unwrap_or(0) > current.priority.unwrap_or(0))
        {
            best = Some(role.clone());
        }
    });
    best
}

/// Collects *all* call-arg roles for `param_idx` whose domain matches (and
/// whose role is in `roles`, when non-empty), sorted by descending priority
/// then by param index. This is the non-selective counterpart to
/// [`find_best_call_arg_role_for_param`] for callers that need every matching
/// role rather than just the best.
pub fn collect_call_arg_roles_for_param(
    signature: &LuaSignature,
    param_idx: usize,
    domain: &str,
    roles: &[&str],
) -> Vec<LuaCallArgRole> {
    let mut roles_out = Vec::new();
    signature.visit_call_arg_roles_for_param(param_idx, &mut |role| {
        if role.domain != domain {
            return;
        }
        if !roles.is_empty() && !roles.iter().any(|candidate| *candidate == role.role) {
            return;
        }
        roles_out.push(role.clone());
    });
    roles_out.sort_by_key(|role| {
        (
            role.param_idx,
            std::cmp::Reverse(role.priority.unwrap_or(0)),
        )
    });
    roles_out
}

// ---------------------------------------------------------------------------
// Signature-level standalone attribute helpers.
// ---------------------------------------------------------------------------

/// Returns the standalone attribute uses attached directly to a signature
/// (i.e. attributes stored against `LuaSemanticDeclId::Signature(signature_id)`
/// in the property index), if any.
///
/// This reads existing storage only; it does not synthesize or scan
/// class/type-level attributes. Use [`find_signature_attribute_use`] for a
/// single-name lookup.
pub fn signature_attribute_uses(
    db: &DbIndex,
    signature_id: LuaSignatureId,
) -> Option<&[crate::LuaAttributeUse]> {
    let property = db
        .get_property_index()
        .get_property(&crate::LuaSemanticDeclId::Signature(signature_id))?;
    property.attribute_uses().map(|uses| uses.as_slice())
}

/// Finds a standalone attribute use attached directly to a signature by name.
///
/// Convenience wrapper over [`signature_attribute_uses`] +
/// [`LuaCommonProperty::find_attribute_use`]. Returns `None` when the
/// signature has no standalone attributes or none match `attribute_name`.
pub fn find_signature_attribute_use<'a>(
    db: &'a DbIndex,
    signature_id: LuaSignatureId,
    attribute_name: &str,
) -> Option<&'a crate::LuaAttributeUse> {
    db.get_property_index()
        .get_property(&crate::LuaSemanticDeclId::Signature(signature_id))?
        .find_attribute_use(attribute_name)
}

/// Returns the signature id that owns the standalone attributes attached to
/// the given semantic-decl owner, when the owner resolves back to a signature
/// via the property index's signature-owner map.
///
/// This bridges `LuaSemanticDeclId::Signature(...)` aliases added through
/// [`LuaPropertyIndex::add_owner_map`] back to the canonical signature, so
/// callers that hold an alias owner id can still reach the owning signature.
pub fn signature_owner_for(
    db: &DbIndex,
    owner_id: &crate::LuaSemanticDeclId,
) -> Option<LuaSignatureId> {
    db.get_property_index().get_signature_owner(owner_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        LuaAttributeUse, LuaType, LuaTypeDeclId, VirtualWorkspace,
        db_index::signature::{CALL_ARG_ATTRIBUTE, LuaDocParamInfo, LuaSignature},
    };
    use smol_str::SmolStr;

    fn call_arg_attribute(domain: &str, role: &str, priority: Option<i64>) -> LuaAttributeUse {
        let mut args = vec![
            (
                "domain".to_string(),
                Some(LuaType::DocStringConst(SmolStr::new(domain).into())),
            ),
            (
                "role".to_string(),
                Some(LuaType::DocStringConst(SmolStr::new(role).into())),
            ),
        ];
        if let Some(priority) = priority {
            args.push((
                "priority".to_string(),
                Some(LuaType::DocIntegerConst(priority)),
            ));
        }
        LuaAttributeUse::new(LuaTypeDeclId::global(CALL_ARG_ATTRIBUTE), args)
    }

    fn signature_with_param_roles(param_name: &str, roles: Vec<LuaAttributeUse>) -> LuaSignature {
        let mut signature = LuaSignature::new();
        signature.params.push(param_name.to_string());
        signature.param_docs.insert(
            0,
            LuaDocParamInfo {
                name: param_name.to_string(),
                type_ref: LuaType::String,
                default_value: None,
                nullable: false,
                description: None,
                attributes: Some(roles),
            },
        );
        signature
    }

    #[test]
    fn find_best_call_arg_role_picks_highest_priority() {
        let signature = signature_with_param_roles(
            "name",
            vec![
                call_arg_attribute(GMOD_DOMAIN_VGUI_PANEL, "define", Some(1)),
                call_arg_attribute(GMOD_DOMAIN_VGUI_PANEL, "table", Some(5)),
                call_arg_attribute(GMOD_DOMAIN_VGUI_PANEL, "base", Some(3)),
            ],
        );

        let best = find_best_call_arg_role_for_param(
            &signature,
            0,
            GMOD_DOMAIN_VGUI_PANEL,
            &["define", "table", "base"],
        );
        let best = best.expect("a matching role exists");
        assert_eq!(best.role, "table");
        assert_eq!(best.priority, Some(5));
    }

    #[test]
    fn find_best_call_arg_role_filters_by_domain_and_role() {
        let signature = signature_with_param_roles(
            "name",
            vec![
                call_arg_attribute(GMOD_DOMAIN_VGUI_PANEL, "define", Some(5)),
                call_arg_attribute(GMOD_DOMAIN_DERMA_SKIN, "define", Some(99)),
                call_arg_attribute(GMOD_DOMAIN_VGUI_PANEL, "base", Some(2)),
            ],
        );

        // Wrong domain ignored even with higher priority.
        let best =
            find_best_call_arg_role_for_param(&signature, 0, GMOD_DOMAIN_VGUI_PANEL, &["define"]);
        let best = best.expect("a matching role exists");
        assert_eq!(best.role, "define");
        assert_eq!(best.priority, Some(5));

        // Wrong role ignored.
        let none = find_best_call_arg_role_for_param(
            &signature,
            0,
            GMOD_DOMAIN_VGUI_PANEL,
            &["nonexistent"],
        );
        assert!(none.is_none(), "no role matches the role filter");
    }

    #[test]
    fn find_best_call_arg_role_empty_roles_matches_any_role_in_domain() {
        let signature = signature_with_param_roles(
            "name",
            vec![
                call_arg_attribute(GMOD_DOMAIN_HOOK, "add", Some(1)),
                call_arg_attribute(GMOD_DOMAIN_HOOK, "remove", Some(4)),
            ],
        );

        let best = find_best_call_arg_role_for_param(&signature, 0, GMOD_DOMAIN_HOOK, &[]);
        let best = best.expect("a matching role exists");
        assert_eq!(best.role, "remove");
        assert_eq!(best.priority, Some(4));
    }

    #[test]
    fn find_best_call_arg_role_treats_missing_priority_as_zero() {
        let signature = signature_with_param_roles(
            "name",
            vec![
                call_arg_attribute(GMOD_DOMAIN_LOAD, "include", None),
                call_arg_attribute(GMOD_DOMAIN_LOAD, "require", Some(-1)),
            ],
        );

        // Missing priority (0) beats explicit -1.
        let best = find_best_call_arg_role_for_param(&signature, 0, GMOD_DOMAIN_LOAD, &[]);
        let best = best.expect("a matching role exists");
        assert_eq!(best.role, "include");
        assert_eq!(best.priority, None);
    }

    #[test]
    fn collect_call_arg_roles_returns_all_sorted_by_priority() {
        let signature = signature_with_param_roles(
            "name",
            vec![
                call_arg_attribute(GMOD_DOMAIN_COLOR, "r", Some(1)),
                call_arg_attribute(GMOD_DOMAIN_COLOR, "b", Some(9)),
                call_arg_attribute(GMOD_DOMAIN_COLOR, "g", Some(3)),
                call_arg_attribute(GMOD_DOMAIN_VGUI_PANEL, "define", Some(100)),
            ],
        );

        let roles = collect_call_arg_roles_for_param(&signature, 0, GMOD_DOMAIN_COLOR, &[]);
        assert_eq!(roles.len(), 3);
        // Descending priority.
        assert_eq!(roles[0].role, "b");
        assert_eq!(roles[1].role, "g");
        assert_eq!(roles[2].role, "r");
    }

    #[test]
    fn find_best_call_arg_role_from_type_filters_by_domain_and_role() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@meta
            ---@attribute call_arg(domain: string, role: string, priority: integer?)

            ---@[call_arg("gmod.load", "include", 3)]
            ---@param path string
            function LoadPath(path) end
            "#,
        );

        let db = ws.analysis.compilation.get_db();
        let (signature_id, _signature) = db
            .get_signature_index()
            .iter()
            .next()
            .expect("at least one signature is defined");
        let callee_type = LuaType::Signature(*signature_id);

        let best =
            find_best_call_arg_role_from_type(db, &callee_type, 0, GMOD_DOMAIN_LOAD, &["include"]);
        let best = best.expect("matching role exists on the signature type");
        assert_eq!(best.role, "include");
        assert_eq!(best.priority, Some(3));

        let none =
            find_best_call_arg_role_from_type(db, &callee_type, 0, GMOD_DOMAIN_COLOR, &["include"]);
        assert!(none.is_none(), "wrong domain must not match");
    }

    #[test]
    fn find_best_call_arg_role_from_type_empty_roles_matches_any_role_in_domain() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@meta
            ---@attribute call_arg(domain: string, role: string, priority: integer?)

            ---@[call_arg("gmod.hook", "add", 1)]
            ---@[call_arg("gmod.hook", "remove", 7)]
            ---@param name string
            function HookName(name) end
            "#,
        );

        let db = ws.analysis.compilation.get_db();
        let (signature_id, _signature) = db
            .get_signature_index()
            .iter()
            .next()
            .expect("at least one signature is defined");
        let callee_type = LuaType::Signature(*signature_id);

        let best = find_best_call_arg_role_from_type(db, &callee_type, 0, GMOD_DOMAIN_HOOK, &[]);
        let best = best.expect("empty role filter matches any role in the domain");
        assert_eq!(best.role, "remove");
        assert_eq!(best.priority, Some(7));
    }

    // -----------------------------------------------------------------------
    // Signature standalone attribute helpers (via VirtualWorkspace so the
    // property index is populated through the real analyzer pipeline).
    // -----------------------------------------------------------------------

    #[test]
    fn signature_attribute_helpers_find_standalone_attribute() {
        // Signature-level standalone attributes follow the same convention as
        // the existing `call_arg` attribute: a single-name attribute declared
        // via `---@attribute <name>(...)` and referenced via `---@[<name>(...)]`
        // attached to the function (not to a `---@param`/`---@return` tag, which
        // routes to param/return attribute storage instead). The GMod domain
        // constant is passed as a string argument value, matching how
        // `@call_arg("gmod.load", "include")` carries the `gmod.load` domain.
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@meta
            ---@attribute self_guard(member: string)

            ---@param member string
            ---@[self_guard("gmod.self_guard")]
            function GuardSpawn(member) end
            "#,
        );

        let db = ws.analysis.compilation.get_db();
        let signature_index = db.get_signature_index();
        // Exactly one signature should be defined in the workspace.
        let (signature_id, _signature) = signature_index
            .iter()
            .next()
            .expect("at least one signature is defined");

        let uses = signature_attribute_uses(db, *signature_id);
        let uses = uses.expect("signature has standalone attributes");
        // The attribute id resolves to the single declared name `self_guard`.
        assert_eq!(uses.len(), 1);
        assert_eq!(uses[0].id.get_name(), "self_guard");
        // The GMod domain is carried as a string argument value.
        let domain_arg = uses[0].get_param_by_name("member");
        let domain_arg = domain_arg.expect("member arg is present");
        match domain_arg {
            crate::LuaType::DocStringConst(value) | crate::LuaType::StringConst(value) => {
                assert_eq!(value.as_str(), GMOD_DOMAIN_SELF_GUARD);
            }
            other => panic!("expected string const domain arg, got {other:?}"),
        }

        // The helper locates the attribute by its resolved name.
        let found = find_signature_attribute_use(db, *signature_id, "self_guard");
        let found = found.expect("find_signature_attribute_use locates the attribute");
        assert_eq!(found.id.get_name(), "self_guard");
    }

    #[test]
    fn signature_attribute_helpers_return_none_when_absent() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@param member string
            function NoAttributes(member) end
            "#,
        );

        let db = ws.analysis.compilation.get_db();
        let (signature_id, _signature) = db
            .get_signature_index()
            .iter()
            .next()
            .expect("at least one signature is defined");

        assert!(signature_attribute_uses(db, *signature_id).is_none());
        assert!(find_signature_attribute_use(db, *signature_id, GMOD_DOMAIN_SELF_GUARD).is_none());
    }

    #[test]
    fn class_level_attributes_are_not_surfaced_by_signature_helpers() {
        // A ---@class with an attribute must not be returned by the
        // signature-level helpers, confirming Phase 1 only models
        // signature standalone attributes.
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@meta
            ---@attribute gmod.member_guard(member: string)

            ---@[gmod.member_guard("Spawn")]
            ---@class SpawnGuard
            "#,
        );

        let db = ws.analysis.compilation.get_db();
        // No function signatures are defined.
        assert!(
            db.get_signature_index().iter().next().is_none(),
            "no signatures defined in this fixture"
        );
        // Therefore no signature-level attribute can be resolved.
        for (signature_id, _) in db.get_signature_index().iter() {
            assert!(
                signature_attribute_uses(db, *signature_id).is_none(),
                "class attribute must not leak into signature storage"
            );
        }
    }

    #[test]
    fn gmod_call_arg_domains_are_unique_and_sorted() {
        let mut copied: Vec<&str> = GMOD_CALL_ARG_DOMAINS.to_vec();
        let mut sorted = copied.clone();
        sorted.sort_unstable();
        // Already sorted.
        assert_eq!(copied, sorted, "GMOD_CALL_ARG_DOMAINS is sorted");
        copied.sort_unstable();
        copied.dedup();
        assert_eq!(
            copied.len(),
            GMOD_CALL_ARG_DOMAINS.len(),
            "GMOD_CALL_ARG_DOMAINS has no duplicates"
        );
    }
}
