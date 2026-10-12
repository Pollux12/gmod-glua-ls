#[cfg(test)]
mod tests {
    use crate::{
        LuaAstNode, LuaCallExpr, LuaIndexExpr, LuaIndexKey, LuaNameExpr, LuaParser, LuaSyntaxTree,
        LuaTableField, ParserConfig, PathTrait, path_segment, path_segment_string_key,
        path_segments, split_last_path_segment,
    };

    fn get_tree(code: &str) -> LuaSyntaxTree {
        let config = ParserConfig::default();

        LuaParser::parse(code, config)
    }

    #[test]
    fn test_call_access_path() {
        let code = "call.ddd()";
        let tree = get_tree(code);
        let root = tree.get_chunk_node();
        let call_expr = root.descendants::<LuaCallExpr>().next().unwrap();
        assert_eq!(call_expr.get_access_path().unwrap(), "call.ddd");
    }

    #[test]
    fn test_call_access_path2() {
        let code = "call[1].aaa.bbb.ccc()";
        let tree = get_tree(code);
        let root = tree.get_chunk_node();
        let call_expr = root.descendants::<LuaCallExpr>().next().unwrap();
        assert_eq!(call_expr.get_access_path().unwrap(), "call.1.aaa.bbb.ccc");
    }

    #[test]
    fn test_name_access_path() {
        let code = "local a = name";
        let tree = get_tree(code);
        let root = tree.get_chunk_node();
        let name_expr = root.descendants::<LuaNameExpr>().next().unwrap();
        assert_eq!(name_expr.get_access_path().unwrap(), "name");
    }

    #[test]
    fn test_index_expr_access_path() {
        let code = "local a = name.bbb.ccc";
        let tree = get_tree(code);
        let root = tree.get_chunk_node();
        let index_expr = root.descendants::<LuaIndexExpr>().next().unwrap();
        assert_eq!(index_expr.get_access_path().unwrap(), "name.bbb.ccc");
    }

    #[test]
    fn test_index_expr_access_path2() {
        let code = "local a = name[okok.yes]";
        let tree = get_tree(code);
        let root = tree.get_chunk_node();
        let index_expr = root.descendants::<LuaIndexExpr>().next().unwrap();
        assert_eq!(index_expr.get_access_path().unwrap(), "name.[okok.yes]");
    }

    #[test]
    fn test_index_expr_get_index_key_with_whitespace() {
        let code = "local a = t[ 1 ]";
        let tree = get_tree(code);
        let root = tree.get_chunk_node();
        let index_expr = root.descendants::<LuaIndexExpr>().next().unwrap();
        let key = index_expr.get_index_key().unwrap();
        assert!(matches!(key, LuaIndexKey::Integer(_)));
    }

    #[test]
    fn test_index_expr_get_index_key_compact() {
        let code = "local a = t[1]";
        let tree = get_tree(code);
        let root = tree.get_chunk_node();
        let index_expr = root.descendants::<LuaIndexExpr>().next().unwrap();
        let key = index_expr.get_index_key().unwrap();
        assert!(matches!(key, LuaIndexKey::Integer(_)));
    }

    #[test]
    fn test_index_expr_get_index_key_string_with_whitespace() {
        let code = "local a = t[ 'foo' ]";
        let tree = get_tree(code);
        let root = tree.get_chunk_node();
        let index_expr = root.descendants::<LuaIndexExpr>().next().unwrap();
        let key = index_expr.get_index_key().unwrap();
        assert!(matches!(key, LuaIndexKey::String(_)));
    }

    #[test]
    fn test_table_field_get_field_key_with_whitespace() {
        let code = "local a = { [ 1 ] = 'x' }";
        let tree = get_tree(code);
        let root = tree.get_chunk_node();
        let field = root.descendants::<LuaTableField>().next().unwrap();
        let key = field.get_field_key().unwrap();
        assert!(matches!(key, LuaIndexKey::Integer(_)));
    }

    #[test]
    fn test_table_field_get_field_key_compact() {
        let code = "local a = { [1] = 'x' }";
        let tree = get_tree(code);
        let root = tree.get_chunk_node();
        let field = root.descendants::<LuaTableField>().next().unwrap();
        let key = field.get_field_key().unwrap();
        assert!(matches!(key, LuaIndexKey::Integer(_)));
    }

    fn index_access_path(code: &str) -> String {
        let tree = get_tree(code);
        let root = tree.get_chunk_node();
        let index_expr = root.descendants::<LuaIndexExpr>().next().unwrap();
        index_expr.get_access_path().unwrap().to_string()
    }

    #[test]
    fn a_string_key_spelled_as_a_name_shares_the_names_path() {
        assert_eq!(index_access_path(r#"local a = t["b"].c"#), "t.b.c");
        assert_eq!(index_access_path("local a = t.b.c"), "t.b.c");
    }

    #[test]
    fn a_string_key_no_name_could_spell_keeps_its_own_segment() {
        assert_eq!(index_access_path(r#"local a = t["b.c"]"#), r#"t.["b.c"]"#);
        assert_eq!(index_access_path(r#"local a = t["1"]"#), r#"t.["1"]"#);
        assert_eq!(index_access_path("local a = t[1]"), "t.1");
    }

    #[test]
    fn a_bracketed_segment_reads_back_as_the_key_it_was_made_from() {
        for key in [
            "b.c",
            "1",
            "-2",
            "",
            "[x]",
            "a[b",
            r#"quote " and slash \"#,
            "plain",
        ] {
            let segment = path_segment(key);
            let read_back =
                path_segment_string_key(&segment).unwrap_or_else(|| segment.to_string());
            assert_eq!(read_back, key, "{segment}");
        }
    }

    #[test]
    fn a_path_splits_only_between_segments() {
        let path = format!("t.{}.[a.b].c", path_segment(r#"x.y"z"#));
        assert_eq!(
            path_segments(&path).collect::<Vec<_>>(),
            ["t", path_segment(r#"x.y"z"#).as_str(), "[a.b]", "c"]
        );
        assert_eq!(
            split_last_path_segment(r#"t.["a.b"]"#),
            Some(("t", r#"["a.b"]"#))
        );
        assert_eq!(split_last_path_segment("t"), None);

        let opened = format!("t.{}.c", path_segment("a[b"));
        assert_eq!(
            path_segments(&opened).collect::<Vec<_>>(),
            ["t", path_segment("a[b").as_str(), "c"]
        );
    }
}
