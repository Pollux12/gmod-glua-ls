#[cfg(test)]
mod tests {
    use crate::{
        LuaAstNode, LuaCallExpr, LuaIndexExpr, LuaIndexKey, LuaNameExpr, LuaParser, LuaSyntaxTree,
        LuaTableField, ParserConfig, PathTrait,
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
}
