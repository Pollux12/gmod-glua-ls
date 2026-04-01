#[cfg(test)]
mod test {
    use crate::config::Emmyrc;

    #[test]
    fn test_default_ignore_dir_defaults_resolve_to_4_globs() {
        let emmyrc = Emmyrc::default();

        assert!(emmyrc.workspace.use_default_ignores);
        let resolved = emmyrc.workspace.resolve_ignore_dir_defaults();
        assert_eq!(resolved.len(), 4);
        assert!(resolved.contains(&"**/gmod_wire_expression2/**".to_string()));
        assert!(resolved.contains(&"**/wire_expression*.lua".to_string()));
        assert!(resolved.contains(&"**/tests/**".to_string()));
        assert!(resolved.contains(&"**/test/**".to_string()));
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
        // Defaults still present as entries even when disabled (4 built-ins)
        assert_eq!(emmyrc.workspace.resolve_ignore_dir_defaults().len(), 4);
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
    fn test_legacy_string_array_replaces_defaults() {
        let json = r#"{
            "workspace": {
                "ignoreDirDefaults": ["**/custom/**"],
                "useDefaultIgnores": true
            }
        }"#;

        let emmyrc: Emmyrc = serde_json::from_str(json).unwrap();

        assert!(emmyrc.workspace.use_default_ignores);
        let resolved = emmyrc.workspace.resolve_ignore_dir_defaults();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0], "**/custom/**");
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
        // Defaults resolve to 4 built-in patterns
        assert_eq!(emmyrc.workspace.resolve_ignore_dir_defaults().len(), 4);
    }

    #[test]
    fn test_object_disable_by_id_removes_builtin() {
        let json = r#"{
            "workspace": {
                "ignoreDirDefaults": [
                    { "id": "tests", "disabled": true }
                ]
            }
        }"#;

        let emmyrc: Emmyrc = serde_json::from_str(json).unwrap();
        let resolved = emmyrc.workspace.resolve_ignore_dir_defaults();

        assert_eq!(
            resolved.len(),
            3,
            "should have 3 entries after disabling 'tests'"
        );
        assert!(!resolved.contains(&"**/tests/**".to_string()));
        assert!(resolved.contains(&"**/test/**".to_string()));
        assert!(resolved.contains(&"**/gmod_wire_expression2/**".to_string()));
    }

    #[test]
    fn test_object_override_changes_builtin_glob() {
        let json = r#"{
            "workspace": {
                "ignoreDirDefaults": [
                    { "id": "test", "glob": "**/my_tests/**" }
                ]
            }
        }"#;

        let emmyrc: Emmyrc = serde_json::from_str(json).unwrap();
        let resolved = emmyrc.workspace.resolve_ignore_dir_defaults();

        assert_eq!(resolved.len(), 4);
        assert!(!resolved.contains(&"**/test/**".to_string()));
        assert!(resolved.contains(&"**/my_tests/**".to_string()));
    }

    #[test]
    fn test_custom_object_entry_adds_new_default() {
        let json = r#"{
            "workspace": {
                "ignoreDirDefaults": [
                    { "id": "my-custom", "glob": "**/custom_ignore/**" }
                ]
            }
        }"#;

        let emmyrc: Emmyrc = serde_json::from_str(json).unwrap();
        let resolved = emmyrc.workspace.resolve_ignore_dir_defaults();

        assert_eq!(resolved.len(), 5, "should have 4 built-ins + 1 custom");
        assert!(resolved.contains(&"**/custom_ignore/**".to_string()));
    }

    #[test]
    fn test_use_default_ignores_false_prevents_applying_resolved_defaults() {
        // useDefaultIgnores: false means the *runtime* ignores the resolved list.
        // The resolved list itself may still contain entries; enforcement is in
        // calculate_include_and_exclude, not in resolve_ignore_dir_defaults.
        let json = r#"{
            "workspace": {
                "useDefaultIgnores": false,
                "ignoreDirDefaults": [
                    { "id": "my-custom", "glob": "**/custom_ignore/**" }
                ]
            }
        }"#;

        let emmyrc: Emmyrc = serde_json::from_str(json).unwrap();
        assert!(!emmyrc.workspace.use_default_ignores);
        // resolve still returns a list but the VFS layer won't apply it
        let resolved = emmyrc.workspace.resolve_ignore_dir_defaults();
        assert!(!resolved.is_empty());
    }

    #[test]
    fn test_mixed_strings_and_objects_appends_strings() {
        let json = r#"{
            "workspace": {
                "ignoreDirDefaults": [
                    { "id": "tests", "disabled": true },
                    "**/legacy_tests/**"
                ]
            }
        }"#;

        let emmyrc: Emmyrc = serde_json::from_str(json).unwrap();
        let resolved = emmyrc.workspace.resolve_ignore_dir_defaults();

        // 4 built-ins minus the disabled "tests" = 3, plus the legacy string = 4
        assert_eq!(resolved.len(), 4);
        assert!(!resolved.contains(&"**/tests/**".to_string()));
        assert!(resolved.contains(&"**/legacy_tests/**".to_string()));
    }

    #[test]
    fn test_auto_load_annotations_null_deserializes() {
        let json = r#"{
            "gmod": {
                "autoLoadAnnotations": null
            }
        }"#;

        let emmyrc: Emmyrc = serde_json::from_str(json).unwrap();
        assert_eq!(emmyrc.gmod.auto_load_annotations, None);
    }
}
