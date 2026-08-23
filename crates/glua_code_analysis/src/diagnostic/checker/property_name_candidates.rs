use std::collections::HashSet;

use crate::{DbIndex, LuaSemanticDeclId};

use super::access_invisible::property_can_report_access_invisible;
use super::deprecated::property_can_report_deprecated;
use super::readonly_check::property_can_report_readonly;

/// The names a property-driven checker could possibly report on. Derived from
/// the property index, so it is the same for every file in a run.
#[derive(Debug, Default)]
pub struct PrecomputedPropertyNameCandidates {
    pub deprecated: HashSet<String>,
    pub readonly: HashSet<String>,
    pub access_invisible: HashSet<String>,
}

pub fn precompute_property_name_candidates(db: &DbIndex) -> PrecomputedPropertyNameCandidates {
    let mut candidates = PrecomputedPropertyNameCandidates::default();

    for (owner_id, property) in db.get_property_index().iter_owner_properties() {
        let deprecated = property_can_report_deprecated(property);
        let readonly = property_can_report_readonly(property);
        let access_invisible = property_can_report_access_invisible(property);
        if !deprecated && !readonly && !access_invisible {
            continue;
        }

        match owner_id {
            LuaSemanticDeclId::LuaDecl(decl_id) => {
                let Some(decl) = db.get_decl_index().get_decl(decl_id) else {
                    continue;
                };
                let name = decl.get_name();
                if deprecated {
                    candidates.deprecated.insert(name.to_string());
                }
                if readonly {
                    candidates.readonly.insert(name.to_string());
                }
                if access_invisible {
                    candidates.access_invisible.insert(name.to_string());
                }
            }
            LuaSemanticDeclId::Member(member_id) => {
                let Some(name) = db
                    .get_member_index()
                    .get_member(member_id)
                    .and_then(|member| member.get_key().get_name())
                else {
                    continue;
                };
                if deprecated {
                    candidates.deprecated.insert(name.to_string());
                }
                if readonly {
                    candidates.readonly.insert(name.to_string());
                }
                if access_invisible {
                    candidates.access_invisible.insert(name.to_string());
                }
            }
            // A type declaration names no runtime access, so the visibility
            // checker does not take it.
            LuaSemanticDeclId::TypeDecl(type_decl_id) => {
                if deprecated {
                    candidates
                        .deprecated
                        .insert(type_decl_id.get_name().to_string());
                    candidates
                        .deprecated
                        .insert(type_decl_id.get_simple_name().to_string());
                }
                if readonly {
                    candidates
                        .readonly
                        .insert(type_decl_id.get_name().to_string());
                    candidates
                        .readonly
                        .insert(type_decl_id.get_simple_name().to_string());
                }
            }
            LuaSemanticDeclId::Signature(_) => {}
        }
    }

    candidates
}
