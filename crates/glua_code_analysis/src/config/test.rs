#[cfg(test)]
mod test {
    use crate::config::Emmyrc;

    #[test]
    fn test_default_ignore_dir_defaults() {
        let emmyrc = Emmyrc::default();

        // Check that default ignores are set (4 patterns: E2 files, wire_expression files, tests, test)
        assert!(emmyrc.workspace.use_default_ignores);
        assert_eq!(emmyrc.workspace.ignore_dir_defaults.len(), 4);
        assert!(
            emmyrc
                .workspace
                .ignore_dir_defaults
                .contains(&"**/gmod_wire_expression2/**".to_string())
        );
        assert!(
            emmyrc
                .workspace
                .ignore_dir_defaults
                .contains(&"**/wire_expression*.lua".to_string())
        );
        assert!(
            emmyrc
                .workspace
                .ignore_dir_defaults
                .contains(&"**/tests/**".to_string())
        );
        assert!(
            emmyrc
                .workspace
                .ignore_dir_defaults
                .contains(&"**/test/**".to_string())
        );
    }

    #[test]
    fn test_default_ignore_dir_is_empty() {
        let emmyrc = Emmyrc::default();

        // User ignore_dir should be empty by default
        assert!(emmyrc.workspace.ignore_dir.is_empty());
    }

    #[test]
    fn test_use_default_ignores_can_be_disabled() {
        let json = r#"{
            "workspace": {
                "useDefaultIgnores": false
            }
        }"#;

        let emmyrc: Emmyrc = serde_json::from_str(json).unwrap();

        assert!(!emmyrc.workspace.use_default_ignores);
        // Defaults should still be present even when disabled (now 4 patterns)
        assert_eq!(emmyrc.workspace.ignore_dir_defaults.len(), 4);
    }

    #[test]
    fn test_user_ignore_dir_can_override() {
        let json = r#"{
            "workspace": {
                "ignoreDir": ["custom_dir"],
                "useDefaultIgnores": false
            }
        }"#;

        let emmyrc: Emmyrc = serde_json::from_str(json).unwrap();

        assert_eq!(emmyrc.workspace.ignore_dir.len(), 1);
        assert_eq!(emmyrc.workspace.ignore_dir[0], "custom_dir");
        assert!(!emmyrc.workspace.use_default_ignores);
    }

    #[test]
    fn test_ignore_dir_defaults_can_be_customized() {
        let json = r#"{
            "workspace": {
                "ignoreDirDefaults": ["**/custom/**"],
                "useDefaultIgnores": true
            }
        }"#;

        let emmyrc: Emmyrc = serde_json::from_str(json).unwrap();

        assert!(emmyrc.workspace.use_default_ignores);
        assert_eq!(emmyrc.workspace.ignore_dir_defaults.len(), 1);
        assert_eq!(emmyrc.workspace.ignore_dir_defaults[0], "**/custom/**");
    }

    #[test]
    fn test_user_ignore_dir_merged_with_defaults() {
        // Simulate the merging that happens in calculate_include_and_exclude
        let json = r#"{
            "workspace": {
                "ignoreDir": ["user_custom_dir"]
            }
        }"#;

        let emmyrc: Emmyrc = serde_json::from_str(json).unwrap();

        assert!(emmyrc.workspace.use_default_ignores);
        assert_eq!(emmyrc.workspace.ignore_dir.len(), 1);
        assert_eq!(emmyrc.workspace.ignore_dir[0], "user_custom_dir");
        // Defaults are separate from user ignores (now 4 patterns)
        assert_eq!(emmyrc.workspace.ignore_dir_defaults.len(), 4);
    }
}
