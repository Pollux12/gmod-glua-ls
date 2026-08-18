use std::{
    collections::{BTreeSet, HashMap},
    path::PathBuf,
};

use crate::{
    EmmyLuaAnalysis, FileId, LuaMember, LuaMemberKey, LuaMemberOwner, LuaTypeFlag, WorkspaceId,
    WorkspaceKind,
};

const MAX_COLLISION_EXAMPLES: usize = 5;

/// Describes definitions shadowed by an earlier configured library root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryDefinitionCollision {
    /// The earlier, authoritative library root.
    pub preferred_root: PathBuf,
    /// The later library root whose colliding definitions are shadowed.
    pub shadowed_root: PathBuf,
    /// The number of colliding global type definitions.
    pub type_collisions: usize,
    /// The number of colliding stable member definitions.
    pub member_collisions: usize,
    /// Up to five deterministically sorted, readable symbol names.
    pub examples: Vec<String>,
}

impl LibraryDefinitionCollision {
    /// Formats the deterministic warning shared by the CLI and language server.
    pub fn warning_message(&self) -> String {
        let examples = if self.examples.is_empty() {
            String::new()
        } else {
            format!("; examples: {}", self.examples.join(", "))
        };
        format!(
            "Library '{}' has duplicate definitions that conflict with '{}'. The earlier \
             entry in .gluarc.json (workspace.library) takes priority ({} types, {} members{}).",
            self.shadowed_root.display(),
            self.preferred_root.display(),
            self.type_collisions,
            self.member_collisions,
            examples,
        )
    }
}

#[derive(Debug, Default)]
struct CollisionAccumulator {
    type_collisions: usize,
    member_collisions: usize,
    examples: BTreeSet<String>,
}

#[derive(Debug, Clone)]
struct LibraryLocation {
    workspace_id: WorkspaceId,
    order: usize,
}

impl EmmyLuaAnalysis {
    /// Returns cross-library definition collisions in configured precedence order.
    pub fn library_definition_collisions(&self) -> Vec<LibraryDefinitionCollision> {
        let db = self.compilation.get_db();
        let module_index = db.get_module_index();
        let mut collisions = HashMap::<(WorkspaceId, WorkspaceId), CollisionAccumulator>::new();

        for type_decl in db.get_type_index().get_all_types() {
            if !type_decl.get_id().is_global() {
                continue;
            }

            let locations = type_decl.get_locations();
            if locations
                .iter()
                .all(|location| location.flag.contains(LuaTypeFlag::Partial))
            {
                continue;
            }

            let library_locations = collect_library_locations(
                module_index,
                locations.iter().map(|location| location.file_id),
            );
            record_collision(
                &mut collisions,
                &library_locations,
                type_decl.get_full_name(),
                CollisionKind::Type,
            );
        }

        let member_index = db.get_member_index();
        for (owner, key) in member_index.iter_current_owner_keys() {
            if !matches!(key, LuaMemberKey::Name(_) | LuaMemberKey::Integer(_)) {
                continue;
            }
            let members = member_index.get_members_for_owner_key(owner, key);
            let library_locations = collect_library_locations(
                module_index,
                members.iter().map(|member| member.get_file_id()),
            );
            let Some(example) = format_member_example(owner, key, &members) else {
                continue;
            };
            record_collision(
                &mut collisions,
                &library_locations,
                &example,
                CollisionKind::Member,
            );
        }

        let mut result = collisions
            .into_iter()
            .filter_map(|((preferred_id, shadowed_id), collision)| {
                let preferred_root = module_index.get_workspace_root(preferred_id)?.to_path_buf();
                let shadowed_root = module_index.get_workspace_root(shadowed_id)?.to_path_buf();
                Some(LibraryDefinitionCollision {
                    preferred_root,
                    shadowed_root,
                    type_collisions: collision.type_collisions,
                    member_collisions: collision.member_collisions,
                    examples: collision
                        .examples
                        .into_iter()
                        .take(MAX_COLLISION_EXAMPLES)
                        .collect(),
                })
            })
            .collect::<Vec<_>>();
        result.sort_by(|left, right| {
            left.preferred_root
                .cmp(&right.preferred_root)
                .then_with(|| left.shadowed_root.cmp(&right.shadowed_root))
        });
        result
    }
}

#[derive(Debug, Clone, Copy)]
enum CollisionKind {
    Type,
    Member,
}

fn collect_library_locations(
    module_index: &crate::LuaModuleIndex,
    file_ids: impl Iterator<Item = FileId>,
) -> Vec<LibraryLocation> {
    let mut by_workspace = HashMap::<WorkspaceId, LibraryLocation>::new();
    for file_id in file_ids {
        let Some(workspace_id) = module_index.get_workspace_id(file_id) else {
            continue;
        };
        if module_index.get_workspace_kind(workspace_id) != WorkspaceKind::Library {
            continue;
        }
        let Some(order) = module_index.workspace_registration_order(workspace_id) else {
            continue;
        };
        by_workspace.entry(workspace_id).or_insert(LibraryLocation {
            workspace_id,
            order,
        });
    }

    let mut locations = by_workspace.into_values().collect::<Vec<_>>();
    locations.sort_by_key(|location| (location.order, location.workspace_id.id));
    locations
}

fn record_collision(
    collisions: &mut HashMap<(WorkspaceId, WorkspaceId), CollisionAccumulator>,
    locations: &[LibraryLocation],
    example: &str,
    kind: CollisionKind,
) {
    let Some(preferred) = locations.first() else {
        return;
    };
    for shadowed in &locations[1..] {
        let collision = collisions
            .entry((preferred.workspace_id, shadowed.workspace_id))
            .or_default();
        match kind {
            CollisionKind::Type => collision.type_collisions += 1,
            CollisionKind::Member => collision.member_collisions += 1,
        }
        collision.examples.insert(example.to_string());
    }
}

fn format_member_example(
    owner: &LuaMemberOwner,
    key: &LuaMemberKey,
    members: &[&LuaMember],
) -> Option<String> {
    let owner_name = match owner {
        LuaMemberOwner::Type(type_id) => type_id.get_name(),
        LuaMemberOwner::GlobalPath(global_id) => global_id.get_name(),
        LuaMemberOwner::LocalUnresolve | LuaMemberOwner::Element(_) => return None,
    };
    let key_name = match key {
        LuaMemberKey::Name(name) => name.to_string(),
        LuaMemberKey::Integer(index) => format!("[{index}]"),
        LuaMemberKey::None | LuaMemberKey::ExprType(_) => return None,
    };
    let separator = if matches!(owner, LuaMemberOwner::Type(_))
        && members
            .iter()
            .any(|member| member.get_feature().is_method_decl())
    {
        ":"
    } else if matches!(key, LuaMemberKey::Integer(_)) {
        ""
    } else {
        "."
    };

    Some(format!("{owner_name}{separator}{key_name}"))
}
