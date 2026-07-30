use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

use crate::read_gamemode_base;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GmodProjectKind {
    Addon,
    Gamemode,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GmodProject {
    pub id: String,
    pub kind: GmodProjectKind,
    pub name: String,
    pub root: PathBuf,
    pub base: Option<String>,
    pub selectable: bool,
}

#[derive(Clone, Debug, Default)]
pub struct GmodWorkspaceTopology {
    projects: Vec<GmodProject>,
}

impl GmodWorkspaceTopology {
    pub fn discover(workspace_roots: &[PathBuf]) -> Self {
        let mut projects_by_root = HashMap::<PathBuf, GmodProject>::new();

        for workspace_root in workspace_roots {
            discover_from_root(workspace_root, &mut projects_by_root);
        }

        let mut projects = projects_by_root.into_values().collect::<Vec<_>>();
        projects.sort_by(|left, right| {
            project_kind_order(left.kind)
                .cmp(&project_kind_order(right.kind))
                .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
                .then_with(|| {
                    normalized_path_key(&left.root).cmp(&normalized_path_key(&right.root))
                })
        });

        Self { projects }
    }

    pub fn projects(&self) -> &[GmodProject] {
        &self.projects
    }

    pub fn addons(&self) -> impl Iterator<Item = &GmodProject> {
        self.projects
            .iter()
            .filter(|project| project.kind == GmodProjectKind::Addon)
    }

    pub fn gamemodes(&self) -> impl Iterator<Item = &GmodProject> {
        self.projects
            .iter()
            .filter(|project| project.kind == GmodProjectKind::Gamemode)
    }

    pub fn primary_gamemodes(&self) -> impl Iterator<Item = &GmodProject> {
        self.gamemodes().filter(|project| project.selectable)
    }

    pub fn project_by_id(&self, id: &str) -> Option<&GmodProject> {
        self.projects.iter().find(|project| project.id == id)
    }

    pub fn project_containing(&self, path: &Path) -> Option<&GmodProject> {
        self.projects
            .iter()
            .filter(|project| path.starts_with(&project.root))
            .max_by_key(|project| project.root.as_os_str().len())
    }

    pub fn gamemode_chain(&self, primary_id: &str) -> Vec<&GmodProject> {
        let Some(primary) = self
            .project_by_id(primary_id)
            .filter(|project| project.kind == GmodProjectKind::Gamemode)
        else {
            return Vec::new();
        };

        let gamemodes_by_name = self
            .gamemodes()
            .map(|project| (project.name.to_ascii_lowercase(), project))
            .collect::<HashMap<_, _>>();
        let mut chain = vec![primary];
        let mut visited = HashSet::from([primary.name.to_ascii_lowercase()]);
        let mut current = primary;

        while let Some(base_name) = current.base.as_deref() {
            let normalized_name = base_name.to_ascii_lowercase();
            if !visited.insert(normalized_name.clone()) {
                break;
            }
            let Some(base) = gamemodes_by_name.get(&normalized_name).copied() else {
                break;
            };
            chain.push(base);
            current = base;
        }

        chain
    }
}

fn discover_from_root(workspace_root: &Path, projects_by_root: &mut HashMap<PathBuf, GmodProject>) {
    if !workspace_root.is_dir() {
        return;
    }

    if is_gamemode_root(workspace_root) {
        insert_project(
            workspace_root,
            GmodProjectKind::Gamemode,
            true,
            projects_by_root,
        );
        discover_gamemode_bases(workspace_root, projects_by_root);
    } else if is_addon_root(workspace_root) {
        insert_project(
            workspace_root,
            GmodProjectKind::Addon,
            true,
            projects_by_root,
        );
    }

    let root_name = file_name_lower(workspace_root);
    if root_name.as_deref() == Some("addons") {
        discover_addons_container(workspace_root, projects_by_root);
        return;
    }
    if root_name.as_deref() == Some("gamemodes") {
        discover_gamemodes_container(workspace_root, projects_by_root);
        return;
    }

    let mut game_roots = Vec::new();
    if root_name.as_deref() == Some("garrysmod") {
        game_roots.push(workspace_root.to_path_buf());
    }
    if workspace_root.join("addons").is_dir() || workspace_root.join("gamemodes").is_dir() {
        game_roots.push(workspace_root.to_path_buf());
    }
    let nested_garrysmod = workspace_root.join("garrysmod");
    if nested_garrysmod.is_dir() {
        game_roots.push(nested_garrysmod);
    }

    game_roots.sort_by_key(|path| normalized_path_key(path));
    game_roots.dedup_by(|left, right| paths_equal(left, right));
    for game_root in game_roots {
        discover_addons_container(&game_root.join("addons"), projects_by_root);
        discover_gamemodes_container(&game_root.join("gamemodes"), projects_by_root);
    }
}

fn discover_addons_container(
    addons_root: &Path,
    projects_by_root: &mut HashMap<PathBuf, GmodProject>,
) {
    for directory in sorted_child_directories(addons_root) {
        insert_project(&directory, GmodProjectKind::Addon, true, projects_by_root);
    }
}

fn discover_gamemodes_container(
    gamemodes_root: &Path,
    projects_by_root: &mut HashMap<PathBuf, GmodProject>,
) {
    for directory in sorted_child_directories(gamemodes_root) {
        if is_gamemode_root(&directory) {
            insert_project(
                &directory,
                GmodProjectKind::Gamemode,
                true,
                projects_by_root,
            );
        }
    }
}

fn sorted_child_directories(root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut directories = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();
    directories.sort_by_key(|path| normalized_path_key(path));
    directories
}

fn insert_project(
    root: &Path,
    kind: GmodProjectKind,
    selectable: bool,
    projects_by_root: &mut HashMap<PathBuf, GmodProject>,
) {
    let key = comparable_path(&canonicalize_or(root));
    if let Some(existing) = projects_by_root.get_mut(&key) {
        existing.selectable |= selectable;
        return;
    }

    let Some(name) = root
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
    else {
        return;
    };
    let root = root.to_path_buf();
    let base = (kind == GmodProjectKind::Gamemode)
        .then(|| read_gamemode_base(&root.join(format!("{name}.txt"))))
        .flatten();

    projects_by_root.insert(
        key,
        GmodProject {
            id: normalized_path_key(&root),
            kind,
            name,
            root,
            base,
            selectable,
        },
    );
}

fn discover_gamemode_bases(
    primary_root: &Path,
    projects_by_root: &mut HashMap<PathBuf, GmodProject>,
) {
    let Some(gamemodes_root) = primary_root.parent() else {
        return;
    };
    let Some(mut current_name) = primary_root
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
    else {
        return;
    };
    let mut visited = HashSet::new();

    loop {
        if !visited.insert(current_name.to_ascii_lowercase()) {
            return;
        }
        let current_root = gamemodes_root.join(&current_name);
        let Some(base_name) = read_gamemode_base(&current_root.join(format!("{current_name}.txt")))
        else {
            return;
        };
        let base_root = gamemodes_root.join(&base_name);
        if !is_gamemode_root(&base_root) {
            return;
        }
        insert_project(
            &base_root,
            GmodProjectKind::Gamemode,
            false,
            projects_by_root,
        );
        current_name = base_name;
    }
}

fn is_gamemode_root(root: &Path) -> bool {
    let Some(name) = root.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    root.join(format!("{name}.txt")).is_file()
}

fn is_addon_root(root: &Path) -> bool {
    root.join("lua").is_dir()
        && !root
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.eq_ignore_ascii_case("garrysmod"))
}

fn file_name_lower(path: &Path) -> Option<String> {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(str::to_ascii_lowercase)
}

fn canonicalize_or(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn comparable_path(path: &Path) -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(path.to_string_lossy().to_ascii_lowercase())
    } else {
        path.to_path_buf()
    }
}

fn normalized_path_key(path: &Path) -> String {
    let normalized = path.to_string_lossy().replace('\\', "/");
    if cfg!(windows) {
        normalized.to_ascii_lowercase()
    } else {
        normalized
    }
}

fn paths_equal(left: &Path, right: &Path) -> bool {
    comparable_path(&canonicalize_or(left)) == comparable_path(&canonicalize_or(right))
}

const fn project_kind_order(kind: GmodProjectKind) -> u8 {
    match kind {
        GmodProjectKind::Addon => 0,
        GmodProjectKind::Gamemode => 1,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_dir() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time should be valid")
            .as_nanos();
        let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("gluals_topology_{nanos}_{counter}"));
        fs::create_dir_all(&root).expect("temp root should be created");
        root
    }

    fn create_addon(root: &Path, name: &str) -> PathBuf {
        let addon = root.join("addons").join(name);
        fs::create_dir_all(addon.join("lua")).expect("addon should be created");
        addon
    }

    fn create_gamemode(root: &Path, name: &str, base: Option<&str>) -> PathBuf {
        let gamemode = root.join("gamemodes").join(name);
        fs::create_dir_all(&gamemode).expect("gamemode should be created");
        let base_line = base
            .map(|base| format!("\"base\" \"{base}\"\n"))
            .unwrap_or_default();
        fs::write(
            gamemode.join(format!("{name}.txt")),
            format!("\"{name}\"\n{{\n{base_line}}}\n"),
        )
        .expect("manifest should be written");
        gamemode
    }

    #[test]
    fn discovers_whole_server_and_resolves_recursive_gamemode_chain() {
        let server = temp_dir();
        let garrysmod = server.join("garrysmod");
        fs::create_dir_all(&garrysmod).expect("garrysmod should be created");
        create_addon(&garrysmod, "example");
        create_gamemode(&garrysmod, "framework", None);
        create_gamemode(&garrysmod, "roleplay", Some("framework"));

        let topology = GmodWorkspaceTopology::discover(std::slice::from_ref(&server));
        let primary = topology
            .gamemodes()
            .find(|project| project.name == "roleplay")
            .expect("roleplay should be discovered");
        let chain = topology
            .gamemode_chain(&primary.id)
            .into_iter()
            .map(|project| project.name.as_str())
            .collect::<Vec<_>>();

        assert_eq!(topology.addons().count(), 1);
        assert_eq!(chain, vec!["roleplay", "framework"]);
        fs::remove_dir_all(server).expect("temp root should be removed");
    }

    #[test]
    fn deduplicates_overlapping_container_and_project_roots() {
        let garrysmod = temp_dir();
        let addon = create_addon(&garrysmod, "example");
        let gamemode = create_gamemode(&garrysmod, "roleplay", None);
        let roots = vec![garrysmod.clone(), garrysmod.join("addons"), addon, gamemode];

        let topology = GmodWorkspaceTopology::discover(&roots);

        assert_eq!(topology.addons().count(), 1);
        assert_eq!(topology.primary_gamemodes().count(), 1);
        fs::remove_dir_all(garrysmod).expect("temp root should be removed");
    }

    #[test]
    fn standalone_gamemode_keeps_base_dependencies_out_of_primary_candidates() {
        let garrysmod = temp_dir();
        create_gamemode(&garrysmod, "framework", None);
        let roleplay = create_gamemode(&garrysmod, "roleplay", Some("framework"));

        let topology = GmodWorkspaceTopology::discover(std::slice::from_ref(&roleplay));
        let primary = topology
            .primary_gamemodes()
            .next()
            .expect("standalone gamemode should be selectable");
        let chain = topology
            .gamemode_chain(&primary.id)
            .into_iter()
            .map(|project| project.name.as_str())
            .collect::<Vec<_>>();

        assert_eq!(topology.primary_gamemodes().count(), 1);
        assert_eq!(chain, vec!["roleplay", "framework"]);
        fs::remove_dir_all(garrysmod).expect("temp root should be removed");
    }

    #[test]
    fn discovers_addons_and_gamemodes_container_roots() {
        let garrysmod = temp_dir();
        create_addon(&garrysmod, "one");
        create_addon(&garrysmod, "two");
        create_gamemode(&garrysmod, "roleplay", None);

        let topology = GmodWorkspaceTopology::discover(&[
            garrysmod.join("addons"),
            garrysmod.join("gamemodes"),
        ]);

        assert_eq!(topology.addons().count(), 2);
        assert_eq!(topology.primary_gamemodes().count(), 1);
        fs::remove_dir_all(garrysmod).expect("temp root should be removed");
    }

    #[test]
    fn discovers_standalone_addon_root() {
        let root = temp_dir();
        let addon = root.join("my_addon");
        fs::create_dir_all(addon.join("lua")).expect("addon should be created");

        let topology = GmodWorkspaceTopology::discover(std::slice::from_ref(&addon));
        let projects = topology.addons().collect::<Vec<_>>();

        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].name, "my_addon");
        assert_eq!(projects[0].root, addon);
        fs::remove_dir_all(root).expect("temp root should be removed");
    }
}
