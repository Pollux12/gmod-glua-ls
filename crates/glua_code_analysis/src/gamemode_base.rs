//! Server-side detection of GMod gamemode base libraries.
//!
//! Garry's Mod gamemodes live under `<gameroot>/gamemodes/<name>/` and carry a
//! `.txt` KeyValues metadata file describing the gamemode. That file may
//! contain a `"base"` field naming a parent gamemode whose folder name is
//! `<base>`. At runtime, gmod loads the parent's code first via
//! `DeriveGamemode("<base>")`, so for accurate static analysis we must resolve
//! the inheritance chain and add each ancestor's gamemode folder as a library.
//!
//! This module:
//!   * scans gamemode folders for top-level `.txt` manifests,
//!   * parses just enough of the KeyValues format to extract the `"base"` field,
//!   * follows the `base` chain (e.g. `darkrp` -> `sandbox` -> `base`),
//!   * returns the absolute folder paths of all ancestor gamemodes that exist
//!     on disk and are not the workspace itself, stopping at `base` and
//!     `sandbox`, which the GMod annotations declare.
//!
//! The detector is intentionally tolerant: malformed KV files are skipped,
//! cycles are broken, and nothing is added when no metadata is found.

use rustc_hash::FxHashSet;
use std::path::{Path, PathBuf};

/// Validate that `name` is a plausible gamemode folder name.
///
/// gmod gamemodes use simple identifiers (lowercase ASCII letters, digits,
/// underscores, hyphens). Anything else — path separators, parent-dir
/// segments, drive prefixes, whitespace — is rejected to prevent a malicious
/// `"base"` value (e.g. `../../etc`) from steering the detector at folders
/// outside `gamemodes/`.
pub(crate) fn is_valid_gamemode_name(name: &str) -> bool {
    if name.is_empty() || name == "." || name == ".." {
        return false;
    }
    name.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Gamemodes the GMod annotations declare. Loading their source as well would
/// define everything they provide twice, so a base chain ends where it reaches one.
const ANNOTATED_GAMEMODES: [&str; 2] = ["base", "sandbox"];

fn is_annotated_gamemode(name: &str) -> bool {
    ANNOTATED_GAMEMODES
        .iter()
        .any(|annotated| annotated.eq_ignore_ascii_case(name))
}

/// Extract the `"base"` value from a gamemode `.txt` KeyValues file.
///
/// Returns `None` when the file is missing, unreadable, malformed, or has an
/// empty `base` (which gmod treats as "no parent", e.g. `base.txt`).
pub fn read_gamemode_base(txt_path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(txt_path).ok()?;
    // Some authoring tools save .txt with a UTF-8 BOM. Strip it before parsing.
    let trimmed = content.strip_prefix('\u{FEFF}').unwrap_or(&content);
    parse_base_field(trimmed)
}

/// A gamemode folder's manifest: `<name>/<name>.txt`, or else the first `.txt`
/// in it, by name, that holds a KeyValues block (`helix-hl2rp/ixhl2rp.txt`).
pub(crate) fn find_gamemode_manifest(dir: &Path) -> Option<PathBuf> {
    preferred_manifest(dir).or_else(|| {
        fallback_manifests(dir).into_iter().find(|path| {
            std::fs::read_to_string(path).is_ok_and(|content| {
                is_keyvalues_block(content.strip_prefix('\u{FEFF}').unwrap_or(&content))
            })
        })
    })
}

/// Whether `input` opens with a named KeyValues block.
fn is_keyvalues_block(input: &str) -> bool {
    let mut tokens = Tokenizer::new(input);
    matches!(tokens.next_token(), Some(Token::Word(_))) && tokens.next_token() == Some(Token::Open)
}

/// Detect base gamemode library paths for a single workspace root.
///
/// The detector handles two layouts:
///   1. **Game install root** (e.g. `.../garrysmod/`): scans `gamemodes/*` for
///      every gamemode and follows each manifest's base chain.
///   2. **Single gamemode root** (e.g. `.../gamemodes/darkrp/`): if the root
///      itself contains a manifest, follows that chain.
///
/// Returned paths:
///   * are absolute,
///   * point at the ancestor gamemode folder (e.g. `.../gamemodes/sandbox`),
///   * exclude the workspace root itself (avoid self-library),
///   * exclude the gamemodes the annotations declare (`base`, `sandbox`),
///   * are deduplicated while preserving discovery order.
pub fn detect_gamemode_base_libraries(workspace_root: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut seen: FxHashSet<PathBuf> = FxHashSet::default();
    let workspace_root_canon = canonicalize_or(workspace_root);

    // Layout 2: workspace root *is* a gamemode folder.
    // Require the parent directory to be named `gamemodes` (ASCII
    // case-insensitive) so we don't accidentally treat unrelated folders
    // as gamemodes.
    if let Some(name) = workspace_root.file_name().and_then(|s| s.to_str())
        && is_valid_gamemode_name(name)
        && let Some(parent) = workspace_root.parent()
        && parent
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|n| n.eq_ignore_ascii_case("gamemodes"))
    {
        walk_chain(
            workspace_root,
            parent,
            &workspace_root_canon,
            &mut out,
            &mut seen,
        );
    }

    // Layout 1: workspace root contains a `gamemodes/` directory.
    let gamemodes_dir = workspace_root.join("gamemodes");
    if gamemodes_dir.is_dir() {
        let entries = std::fs::read_dir(&gamemodes_dir).ok();
        if let Some(entries) = entries {
            // Sort for deterministic output across platforms / file systems.
            let mut folders: Vec<PathBuf> = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect();
            folders.sort();
            for gm_folder in folders {
                let Some(name) = gm_folder.file_name().and_then(|s| s.to_str()) else {
                    continue;
                };
                if !is_valid_gamemode_name(name) {
                    continue;
                }
                walk_chain(
                    &gm_folder,
                    &gamemodes_dir,
                    &workspace_root_canon,
                    &mut out,
                    &mut seen,
                );
            }
        }
    }

    out
}

/// Walk the `base` chain starting from `start_folder`, pushing each *ancestor*
/// gamemode folder into `out` if it exists on disk and is not the workspace
/// itself. `start_folder` is **not** added.
fn walk_chain(
    start_folder: &Path,
    gamemodes_dir: &Path,
    workspace_root_canon: &Path,
    out: &mut Vec<PathBuf>,
    seen: &mut FxHashSet<PathBuf>,
) {
    let mut current = start_folder.to_path_buf();
    let mut visited_names: FxHashSet<String> = FxHashSet::default();
    if let Some(name) = current.file_name().and_then(|s| s.to_str()) {
        visited_names.insert(name.to_string());
    }

    loop {
        let Some(base) = read_gamemode_folder_base(&current) else {
            return;
        };
        // Reject path-traversal / weird names before joining.
        if !is_valid_gamemode_name(&base) || is_annotated_gamemode(&base) {
            return;
        }
        if !visited_names.insert(base.clone()) {
            // cycle
            return;
        }

        let parent_folder = gamemodes_dir.join(&base);
        if !parent_folder.is_dir() {
            return;
        }

        let parent_canon = canonicalize_or(&parent_folder);
        if parent_canon != *workspace_root_canon && seen.insert(parent_canon.clone()) {
            out.push(parent_folder.clone());
        }

        current = parent_folder;
    }
}

/// The `"base"` of a gamemode folder: its `<name>.txt`, or else the first
/// `.txt` in it, by name, that names one.
pub(crate) fn read_gamemode_folder_base(folder: &Path) -> Option<String> {
    match preferred_manifest(folder) {
        Some(preferred) => read_gamemode_base(&preferred),
        None => fallback_manifests(folder)
            .iter()
            .find_map(|path| read_gamemode_base(path)),
    }
}

fn preferred_manifest(folder: &Path) -> Option<PathBuf> {
    let name = folder.file_name()?.to_str()?;
    Some(folder.join(format!("{name}.txt"))).filter(|path| path.is_file())
}

fn fallback_manifests(folder: &Path) -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(folder)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| {
                    path.is_file()
                        && path
                            .extension()
                            .is_some_and(|ext| ext.eq_ignore_ascii_case("txt"))
                })
                .collect()
        })
        .unwrap_or_default();
    candidates.sort();
    candidates
}

fn canonicalize_or(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// Minimal Source-engine KeyValues parser that returns the value of the
/// top-level `"base"` field.
///
/// We do **not** need a full KV parser. We only need to find the `base` token
/// inside the outermost block. Approach:
///   * tokenize: quoted strings, bare words, `{`, `}`, comments (`//`).
///   * descend into the first top-level block (depth 0 -> 1).
///   * inside depth 1, scan key/value pairs at this depth only and pick the
///     first `base` key.
fn parse_base_field(input: &str) -> Option<String> {
    let mut tokens = Tokenizer::new(input);

    // Skip the root key (the gamemode name).
    let _root_key = tokens.next_token()?;
    // Open brace.
    let open = tokens.next_token()?;
    if open != Token::Open {
        return None;
    }

    let mut depth: usize = 1;
    while let Some(tok) = tokens.next_token() {
        match tok {
            Token::Open => {
                depth += 1;
            }
            Token::Close => {
                if depth == 0 {
                    return None;
                }
                depth -= 1;
                if depth == 0 {
                    return None;
                }
            }
            Token::Word(key) => {
                if depth == 1 {
                    // Look ahead: value is either a Word/quoted string or a block.
                    match tokens.next_token()? {
                        Token::Word(value) => {
                            if key.eq_ignore_ascii_case("base") {
                                let trimmed = value.trim();
                                return if trimmed.is_empty() {
                                    None
                                } else {
                                    Some(trimmed.to_string())
                                };
                            }
                        }
                        Token::Open => {
                            depth += 1;
                        }
                        Token::Close => {
                            if depth == 0 {
                                return None;
                            }
                            depth -= 1;
                            if depth == 0 {
                                return None;
                            }
                        }
                    }
                } else {
                    // Inside a nested block — consume value if present so we
                    // don't accidentally treat a value as the next key.
                    match tokens.next_token() {
                        Some(Token::Word(_)) => {}
                        Some(Token::Open) => depth += 1,
                        Some(Token::Close) => {
                            if depth == 0 {
                                return None;
                            }
                            depth -= 1;
                            if depth == 0 {
                                return None;
                            }
                        }
                        None => return None,
                    }
                }
            }
        }
    }

    None
}

#[derive(Debug, PartialEq, Eq)]
enum Token {
    Word(String),
    Open,
    Close,
}

struct Tokenizer<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Tokenizer<'a> {
    fn new(s: &'a str) -> Self {
        Self {
            bytes: s.as_bytes(),
            pos: 0,
        }
    }

    fn next_token(&mut self) -> Option<Token> {
        self.skip_whitespace_and_comments();
        if self.pos >= self.bytes.len() {
            return None;
        }
        let c = self.bytes[self.pos];
        match c {
            b'{' => {
                self.pos += 1;
                Some(Token::Open)
            }
            b'}' => {
                self.pos += 1;
                Some(Token::Close)
            }
            b'"' => {
                self.pos += 1;
                let start = self.pos;
                while self.pos < self.bytes.len() && self.bytes[self.pos] != b'"' {
                    // KV uses backslash escapes only in some implementations;
                    // gmod's gamemode .txt files don't, so a plain scan suffices.
                    if self.bytes[self.pos] == b'\\' && self.pos + 1 < self.bytes.len() {
                        self.pos += 2;
                    } else {
                        self.pos += 1;
                    }
                }
                let raw = &self.bytes[start..self.pos.min(self.bytes.len())];
                if self.pos < self.bytes.len() {
                    self.pos += 1; // consume closing quote
                }
                Some(Token::Word(String::from_utf8_lossy(raw).into_owned()))
            }
            _ => {
                // bare word: read until whitespace / brace / quote
                let start = self.pos;
                while self.pos < self.bytes.len() {
                    let b = self.bytes[self.pos];
                    if b.is_ascii_whitespace() || b == b'{' || b == b'}' || b == b'"' {
                        break;
                    }
                    self.pos += 1;
                }
                let raw = &self.bytes[start..self.pos];
                Some(Token::Word(String::from_utf8_lossy(raw).into_owned()))
            }
        }
    }

    fn skip_whitespace_and_comments(&mut self) {
        loop {
            while self.pos < self.bytes.len() && self.bytes[self.pos].is_ascii_whitespace() {
                self.pos += 1;
            }
            if self.pos + 1 < self.bytes.len()
                && self.bytes[self.pos] == b'/'
                && self.bytes[self.pos + 1] == b'/'
            {
                while self.pos < self.bytes.len() && self.bytes[self.pos] != b'\n' {
                    self.pos += 1;
                }
                continue;
            }
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_dir() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time should be valid")
            .as_nanos();
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("glua_ls_gmbase_test_{nanos}_{n}"));
        fs::create_dir_all(&dir).expect("failed to create temp dir");
        dir
    }

    fn write_gamemode(root: &Path, name: &str, base: Option<&str>) {
        let folder = root.join("gamemodes").join(name);
        fs::create_dir_all(&folder).expect("create gamemode folder");
        let body = match base {
            Some(b) => {
                format!("\"{name}\"\n{{\n\t\"base\"\t\"{b}\"\n\t\"title\"\t\"{name}\"\n}}\n")
            }
            None => format!("\"{name}\"\n{{\n\t\"title\"\t\"{name}\"\n}}\n"),
        };
        fs::write(folder.join(format!("{name}.txt")), body).expect("write gamemode txt");
    }

    #[test]
    fn parses_simple_base_field() {
        let kv = "\"darkrp\"\n{\n\t\"base\"\t\"sandbox\"\n\t\"title\"\t\"DarkRP\"\n}\n";
        assert_eq!(parse_base_field(kv).as_deref(), Some("sandbox"));
    }

    #[test]
    fn empty_base_returns_none() {
        let kv = "\"base\"\n{\n\t\"title\"\t\"Base\"\n\t\"base\"\t\"\"\n}\n";
        assert_eq!(parse_base_field(kv), None);
    }

    #[test]
    fn ignores_base_inside_nested_block() {
        let kv = r#"
"sandbox"
{
    "title"  "Sandbox"
    "settings"
    {
        1
        {
            "name" "physgun_limited"
            "base" "should-not-match"
        }
    }
    "base"   "base"
}
"#;
        assert_eq!(parse_base_field(kv).as_deref(), Some("base"));
    }

    #[test]
    fn tolerates_comments_and_bare_words() {
        let kv = r#"
// a comment
sandbox
{
    base base   // trailing comment
    title "Sandbox"
}
"#;
        assert_eq!(parse_base_field(kv).as_deref(), Some("base"));
    }

    #[test]
    fn returns_none_for_garbage() {
        assert_eq!(parse_base_field(""), None);
        assert_eq!(parse_base_field("not even close"), None);
    }

    fn rename_manifest(root: &Path, name: &str, manifest: &str) {
        let folder = root.join("gamemodes").join(name);
        fs::rename(folder.join(format!("{name}.txt")), folder.join(manifest))
            .expect("rename gamemode manifest");
    }

    #[test]
    fn manifest_mismatched_name_resolves_base() {
        let root = temp_dir();
        write_gamemode(&root, "helix", None);
        write_gamemode(&root, "helix-hl2rp", Some("helix"));
        rename_manifest(&root, "helix-hl2rp", "ixhl2rp.txt");
        assert_eq!(
            detect_gamemode_base_libraries(&root.join("gamemodes/helix-hl2rp")),
            vec![root.join("gamemodes/helix")]
        );
    }

    #[test]
    fn manifest_preferred_name_wins() {
        let root = temp_dir();
        write_gamemode(&root, "ancestor", None);
        write_gamemode(&root, "helix", Some("ancestor"));
        rename_manifest(&root, "helix", "framework.txt");
        write_gamemode(&root, "other", None);
        write_gamemode(&root, "helix-hl2rp", Some("helix"));
        fs::write(
            root.join("gamemodes/helix-hl2rp/a.txt"),
            "other { base other }",
        )
        .unwrap();
        assert_eq!(
            detect_gamemode_base_libraries(&root.join("gamemodes/helix-hl2rp")),
            vec![
                root.join("gamemodes/helix"),
                root.join("gamemodes/ancestor")
            ]
        );
    }

    #[test]
    fn manifest_skips_text_without_base() {
        let root = temp_dir();
        write_gamemode(&root, "helix", None);
        write_gamemode(&root, "helix-hl2rp", Some("helix"));
        rename_manifest(&root, "helix-hl2rp", "ixhl2rp.txt");
        fs::write(
            root.join("gamemodes/helix-hl2rp/a_readme.txt"),
            "readme { title documentation }",
        )
        .unwrap();
        fs::write(
            root.join("gamemodes/helix-hl2rp/b_empty.txt"),
            "empty { base \"\" }",
        )
        .unwrap();
        assert_eq!(
            detect_gamemode_base_libraries(&root.join("gamemodes/helix-hl2rp")),
            vec![root.join("gamemodes/helix")]
        );
    }

    #[test]
    fn manifest_follows_mismatched_ancestor() {
        let root = temp_dir();
        write_gamemode(&root, "ancestor", None);
        write_gamemode(&root, "helix", Some("ancestor"));
        rename_manifest(&root, "helix", "framework.txt");
        write_gamemode(&root, "helix-hl2rp", Some("helix"));
        assert_eq!(
            detect_gamemode_base_libraries(&root.join("gamemodes/helix-hl2rp")),
            vec![
                root.join("gamemodes/helix"),
                root.join("gamemodes/ancestor")
            ]
        );
    }

    #[test]
    fn manifest_fallback_is_sorted() {
        let root = temp_dir();
        write_gamemode(&root, "helix", None);
        write_gamemode(&root, "other", None);
        write_gamemode(&root, "helix-hl2rp", Some("other"));
        rename_manifest(&root, "helix-hl2rp", "z.txt");
        fs::write(
            root.join("gamemodes/helix-hl2rp/a.TXT"),
            "ixhl2rp { base helix }",
        )
        .unwrap();
        assert_eq!(
            detect_gamemode_base_libraries(&root.join("gamemodes/helix-hl2rp")),
            vec![root.join("gamemodes/helix")]
        );
    }

    #[test]
    fn manifest_game_root_discovers_mismatched_name() {
        let root = temp_dir();
        write_gamemode(&root, "helix", None);
        write_gamemode(&root, "helix-hl2rp", Some("helix"));
        rename_manifest(&root, "helix-hl2rp", "ixhl2rp.txt");
        assert_eq!(
            detect_gamemode_base_libraries(&root),
            vec![root.join("gamemodes/helix")]
        );
    }

    #[test]
    fn detect_leaves_annotated_gamemodes_to_the_annotations() {
        let root = temp_dir();
        write_gamemode(&root, "base", Some(""));
        write_gamemode(&root, "sandbox", Some("base"));
        write_gamemode(&root, "darkrp", Some("sandbox"));
        write_gamemode(&root, "helix", Some("sandbox"));
        write_gamemode(&root, "ixhl2rp", Some("helix"));

        assert_eq!(
            detect_gamemode_base_libraries(&root.join("gamemodes").join("darkrp")),
            Vec::<PathBuf>::new()
        );
        assert_eq!(
            detect_gamemode_base_libraries(&root.join("gamemodes").join("ixhl2rp")),
            vec![root.join("gamemodes").join("helix")]
        );
        assert_eq!(
            detect_gamemode_base_libraries(&root),
            vec![root.join("gamemodes").join("helix")]
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn detect_skips_missing_parent_folder() {
        let root = temp_dir();
        // darkrp claims base framework but the framework folder doesn't exist
        write_gamemode(&root, "darkrp", Some("framework"));
        let libs = detect_gamemode_base_libraries(&root);
        assert!(libs.is_empty(), "expected no libs, got {libs:?}");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn detect_handles_workspace_root_being_gamemode() {
        let root = temp_dir();
        // Set up: `<root>/gamemodes/{ancestor,framework}` and treat
        // `<root>/gamemodes/darkrp` as the workspace.
        write_gamemode(&root, "ancestor", Some(""));
        write_gamemode(&root, "framework", Some("ancestor"));
        write_gamemode(&root, "darkrp", Some("framework"));

        let darkrp_root = root.join("gamemodes").join("darkrp");
        assert_eq!(
            detect_gamemode_base_libraries(&darkrp_root),
            vec![
                root.join("gamemodes").join("framework"),
                root.join("gamemodes").join("ancestor"),
            ]
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn detect_breaks_cycles() {
        let root = temp_dir();
        write_gamemode(&root, "a", Some("b"));
        write_gamemode(&root, "b", Some("a"));

        // Should not loop forever; result is just whichever ancestors exist.
        let libs = detect_gamemode_base_libraries(&root);
        // Both folders exist, so each chain walks one step then stops at the
        // cycle. We just assert finiteness and no duplicate entries.
        let mut unique = libs.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(libs.len(), unique.len());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn detect_returns_empty_when_no_gamemodes_dir() {
        let root = temp_dir();
        let libs = detect_gamemode_base_libraries(&root);
        assert!(libs.is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rejects_path_traversal_in_base_value() {
        let root = temp_dir();
        // darkrp's base claims to be `../escape` — must be ignored.
        let folder = root.join("gamemodes").join("darkrp");
        fs::create_dir_all(&folder).unwrap();
        fs::write(
            folder.join("darkrp.txt"),
            "\"darkrp\"\n{\n\t\"base\"\t\"../escape\"\n}\n",
        )
        .unwrap();
        // Also create a real `escape` dir at the root to prove it's not picked up.
        fs::create_dir_all(root.join("escape")).unwrap();

        let libs = detect_gamemode_base_libraries(&root);
        assert!(
            libs.is_empty(),
            "path traversal must be rejected, got {libs:?}"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn layout2_requires_parent_named_gamemodes() {
        let root = temp_dir();
        // Place a gamemode-shaped folder under a non-`gamemodes` parent.
        let gm = root.join("not_gamemodes").join("darkrp");
        fs::create_dir_all(&gm).unwrap();
        fs::write(
            gm.join("darkrp.txt"),
            "\"darkrp\"\n{\n\t\"base\"\t\"sandbox\"\n}\n",
        )
        .unwrap();
        // Sibling sandbox folder so the chain *would* succeed if layout2 fired.
        let sb = root.join("not_gamemodes").join("sandbox");
        fs::create_dir_all(&sb).unwrap();
        fs::write(
            sb.join("sandbox.txt"),
            "\"sandbox\"\n{\n\t\"title\"\t\"Sandbox\"\n}\n",
        )
        .unwrap();

        let libs = detect_gamemode_base_libraries(&gm);
        assert!(
            libs.is_empty(),
            "layout 2 must require parent named `gamemodes`, got {libs:?}"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn validates_gamemode_name() {
        assert!(is_valid_gamemode_name("darkrp"));
        assert!(is_valid_gamemode_name("ttt2"));
        assert!(is_valid_gamemode_name("my-mode_v2"));
        assert!(!is_valid_gamemode_name(""));
        assert!(!is_valid_gamemode_name("."));
        assert!(!is_valid_gamemode_name(".."));
        assert!(!is_valid_gamemode_name("../etc"));
        assert!(!is_valid_gamemode_name("a/b"));
        assert!(!is_valid_gamemode_name("a\\b"));
        assert!(!is_valid_gamemode_name("with space"));
        assert!(!is_valid_gamemode_name("C:"));
    }

    #[test]
    fn read_gamemode_base_handles_real_darkrp_layout() {
        let root = temp_dir();
        let gm = root.join("gamemodes").join("darkrp");
        fs::create_dir_all(&gm).unwrap();
        let body = r#""darkrp"
{
    "base"          "sandbox"
    "title"         "DarkRP"
    "version"       "2.7.0"
    "category"      "rp"
}
"#;
        fs::write(gm.join("darkrp.txt"), body).unwrap();
        assert_eq!(
            read_gamemode_base(&gm.join("darkrp.txt")).as_deref(),
            Some("sandbox")
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn read_gamemode_base_strips_utf8_bom() {
        let root = temp_dir();
        let gm = root.join("gamemodes").join("derived");
        fs::create_dir_all(&gm).unwrap();
        let body = "\u{FEFF}\"derived\"\n{\n\t\"base\"\t\t\"base_gm\"\n}\n";
        fs::write(gm.join("derived.txt"), body).unwrap();

        assert_eq!(
            read_gamemode_base(&gm.join("derived.txt")),
            Some("base_gm".to_string())
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn layout2_accepts_mixed_case_gamemodes_parent() {
        let root = temp_dir();
        let parent = root.join("Gamemodes");
        let gm = parent.join("darkrp");
        let base = parent.join("framework");
        fs::create_dir_all(&gm).unwrap();
        fs::create_dir_all(&base).unwrap();
        fs::write(
            gm.join("darkrp.txt"),
            "\"darkrp\"\n{\n\t\"base\"\t\"framework\"\n}\n",
        )
        .unwrap();
        fs::write(
            base.join("framework.txt"),
            "\"framework\"\n{\n\t\"title\"\t\"Framework\"\n}\n",
        )
        .unwrap();

        let libs = detect_gamemode_base_libraries(&gm);
        assert_eq!(libs, vec![base]);

        let _ = fs::remove_dir_all(root);
    }
}
