use std::collections::{HashMap, HashSet};

use crate::{DiagnosticCode, SemanticModel};

use super::{Checker, DiagnosticContext};

pub struct GmodSystemsChecker;

impl Checker for GmodSystemsChecker {
    const CODES: &[DiagnosticCode] = &[
        DiagnosticCode::GmodUnknownNetMessage,
        DiagnosticCode::GmodDuplicateSystemRegistration,
    ];

    fn check(context: &mut DiagnosticContext, semantic_model: &SemanticModel) {
        let file_id = semantic_model.get_file_id();
        let infer_index = semantic_model.get_db().get_gmod_infer_index();
        let Some(file_metadata) = infer_index.get_system_file_metadata(&file_id) else {
            return;
        };

        let mut known_net_messages = HashSet::new();
        let mut net_registration_count = HashMap::new();
        let mut concommand_registration_count = HashMap::new();
        let mut convar_registration_count = HashMap::new();

        for (_, metadata) in infer_index.iter_system_file_metadata() {
            for site in &metadata.net_add_string_calls {
                if let Some(name) = normalize_name(site.name.as_deref()) {
                    known_net_messages.insert(name.to_string());
                    *net_registration_count.entry(name.to_string()).or_insert(0) += 1;
                }
            }

            for site in &metadata.concommand_add_calls {
                if let Some(name) = normalize_name(site.command_name.as_deref()) {
                    *concommand_registration_count
                        .entry(name.to_string())
                        .or_insert(0) += 1;
                }
            }

            for site in &metadata.convar_create_calls {
                if let Some(name) = normalize_name(site.convar_name.as_deref()) {
                    *convar_registration_count
                        .entry(name.to_string())
                        .or_insert(0) += 1;
                }
            }
        }

        for site in &file_metadata.net_start_calls {
            let Some(name) = normalize_name(site.name.as_deref()) else {
                continue;
            };
            if known_net_messages.contains(name) {
                continue;
            }
            let Some(name_range) = site.name_range else {
                continue;
            };

            context.add_diagnostic(
                DiagnosticCode::GmodUnknownNetMessage,
                name_range,
                t!(
                    "Unknown net message `%{name}` used by net.Start.",
                    name = name
                )
                .to_string(),
                None,
            );
        }

        for site in &file_metadata.net_add_string_calls {
            let Some(name) = normalize_name(site.name.as_deref()) else {
                continue;
            };
            if net_registration_count.get(name).copied().unwrap_or(0) <= 1 {
                continue;
            }
            let Some(name_range) = site.name_range else {
                continue;
            };

            context.add_diagnostic(
                DiagnosticCode::GmodDuplicateSystemRegistration,
                name_range,
                t!(
                    "Duplicate %{kind} name `%{name}` is registered multiple times.",
                    kind = "network string",
                    name = name
                )
                .to_string(),
                None,
            );
        }

        for site in &file_metadata.concommand_add_calls {
            let Some(name) = normalize_name(site.command_name.as_deref()) else {
                continue;
            };
            if concommand_registration_count
                .get(name)
                .copied()
                .unwrap_or(0)
                <= 1
            {
                continue;
            }
            let Some(name_range) = site.name_range else {
                continue;
            };

            context.add_diagnostic(
                DiagnosticCode::GmodDuplicateSystemRegistration,
                name_range,
                t!(
                    "Duplicate %{kind} name `%{name}` is registered multiple times.",
                    kind = "concommand",
                    name = name
                )
                .to_string(),
                None,
            );
        }

        for site in &file_metadata.convar_create_calls {
            let Some(name) = normalize_name(site.convar_name.as_deref()) else {
                continue;
            };
            if convar_registration_count.get(name).copied().unwrap_or(0) <= 1 {
                continue;
            }
            let Some(name_range) = site.name_range else {
                continue;
            };

            context.add_diagnostic(
                DiagnosticCode::GmodDuplicateSystemRegistration,
                name_range,
                t!(
                    "Duplicate %{kind} name `%{name}` is registered multiple times.",
                    kind = "convar",
                    name = name
                )
                .to_string(),
                None,
            );
        }
    }
}

fn normalize_name(name: Option<&str>) -> Option<&str> {
    let name = name?;
    let trimmed = name.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}
