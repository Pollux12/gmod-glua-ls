#[cfg(test)]
mod test {
    use crate::{
        LuaAstNode, LuaComment, LuaDocTagDefaultValue, LuaDocTagField, LuaDocTagParam,
        LuaDocTagReturn, LuaParser, ParserConfig,
    };

    #[allow(unused)]
    fn print_ast(lua_code: &str) {
        let tree = LuaParser::parse(lua_code, ParserConfig::default());
        println!("{:#?}", tree.get_red_root());
    }

    #[test]
    fn test_comment() {
        let code = r#"
        -- 1 comment
        local t = 123 -- 2 comment

        local c = {
            aa = 1123, -- 3 comment
            bb = 123, --[[4 comment]]
            -- 5 comment
            qi = 123,
        }
        "#;

        let tree = LuaParser::parse(code, ParserConfig::default());
        let root = tree.get_chunk_node();
        let mut comment_iter = root.descendants::<LuaComment>();
        let comment_1 = comment_iter.next().unwrap();
        assert_eq!(
            comment_1.get_description().unwrap().get_description_text(),
            "1 comment"
        );
        assert_eq!(
            comment_1.get_owner().unwrap().syntax().text(),
            "local t = 123"
        );

        let comment_2 = comment_iter.next().unwrap();
        assert_eq!(
            comment_2.get_description().unwrap().get_description_text(),
            "2 comment"
        );
        assert_eq!(
            comment_2.get_owner().unwrap().syntax().text(),
            "local t = 123"
        );

        let comment_3 = comment_iter.next().unwrap();
        assert_eq!(
            comment_3.get_description().unwrap().get_description_text(),
            "3 comment"
        );
        assert_eq!(comment_3.get_owner().unwrap().syntax().text(), "aa = 1123");

        let comment_4 = comment_iter.next().unwrap();
        assert_eq!(
            comment_4.get_description().unwrap().get_description_text(),
            "4 comment"
        );
        assert_eq!(comment_4.get_owner().unwrap().syntax().text(), "bb = 123");

        let comment_5 = comment_iter.next().unwrap();
        assert_eq!(
            comment_5.get_description().unwrap().get_description_text(),
            "5 comment"
        );
        assert_eq!(comment_5.get_owner().unwrap().syntax().text(), "qi = 123");
    }

    #[test]
    fn test_description() {
        let code = r#"
--- yeysysf
---@class Test
--- oooo
---@class Test2
---
---hhhh
---@field a string

        "#;

        print_ast(code);
    }

    #[test]
    fn test_inline_default_accessors() {
        let code = r#"
        ---@field ContentsLeft=0 CONTENTS
        ---@field ContentsRight CONTENTS=0
        ---@param retries_left=3 number
        ---@param retries_right number=3
        ---@return boolean=false
        function f() end
        "#;

        let tree = LuaParser::parse(code, ParserConfig::default());
        let root = tree.get_chunk_node();

        let mut field_tags = root.descendants::<LuaDocTagField>();
        let field_left = field_tags.next().unwrap();
        let field_right = field_tags.next().unwrap();
        assert_eq!(
            field_left.get_default_value(),
            Some(LuaDocTagDefaultValue::Number("0".to_string()))
        );
        assert_eq!(
            field_right.get_default_value(),
            Some(LuaDocTagDefaultValue::Number("0".to_string()))
        );

        let mut param_tags = root.descendants::<LuaDocTagParam>();
        let param_left = param_tags.next().unwrap();
        let param_right = param_tags.next().unwrap();
        assert_eq!(
            param_left.get_default_value(),
            Some(LuaDocTagDefaultValue::Number("3".to_string()))
        );
        assert_eq!(
            param_right.get_default_value(),
            Some(LuaDocTagDefaultValue::Number("3".to_string()))
        );

        let return_tag = root.descendants::<LuaDocTagReturn>().next().unwrap();
        let infos = return_tag.get_info_list_with_default();
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].2, Some(LuaDocTagDefaultValue::Boolean(false)));
    }

    #[test]
    fn test_inline_default_accessors_for_named_and_call_defaults() {
        let code = r#"
        ---@param entity Entity=NULL
        ---@param origin Vector=Vector( 0, 0, 0 )
        ---@param angle Angle=Angle( 0, 0, 0 )
        function f(entity, origin, angle) end
        "#;

        let tree = LuaParser::parse(code, ParserConfig::default());
        assert!(tree.get_errors().is_empty(), "{:?}", tree.get_errors());

        let root = tree.get_chunk_node();
        let mut param_tags = root.descendants::<LuaDocTagParam>();

        let entity = param_tags.next().unwrap();
        assert_eq!(
            entity.get_default_value(),
            Some(LuaDocTagDefaultValue::Expression("NULL".to_string()))
        );

        let origin = param_tags.next().unwrap();
        assert_eq!(
            origin.get_default_value(),
            Some(LuaDocTagDefaultValue::Expression(
                "Vector( 0, 0, 0 )".to_string()
            ))
        );

        let angle = param_tags.next().unwrap();
        assert_eq!(
            angle.get_default_value(),
            Some(LuaDocTagDefaultValue::Expression(
                "Angle( 0, 0, 0 )".to_string()
            ))
        );
    }
}
