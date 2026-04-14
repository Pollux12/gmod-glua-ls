use glua_code_analysis::{
    DbIndex, LuaSemanticDeclId, LuaType, LuaTypeDeclId, RenderLevel, humanize_type,
};

use crate::handlers::hover::HoverBuilder;

pub fn build_type_decl_hover(
    builder: &mut HoverBuilder,
    db: &DbIndex,
    type_decl_id: LuaTypeDeclId,
) -> Option<()> {
    let type_decl = db.get_type_index().get_type_decl(&type_decl_id)?;
    let type_description = if type_decl.is_alias() {
        if let Some(origin) = type_decl.get_alias_origin(db, None) {
            let origin_type = humanize_type(db, &origin, builder.detail_render_level);
            format!("(alias) {} = {}", type_decl.get_name(), origin_type)
        } else {
            "".to_string()
        }
    } else if type_decl.is_enum() {
        format!("(enum) {}", type_decl.get_name())
    } else if type_decl.is_attribute() {
        build_attribute(db, type_decl.get_name(), type_decl.get_attribute_type())
    } else {
        let class_name = type_decl.get_full_name();
        // Show super types after the class name (e.g. "paper : ITEM")
        let supers_str = match db.get_type_index().get_super_types(&type_decl_id) {
            Some(supers) if !supers.is_empty() => {
                let super_names: Vec<String> = supers
                    .iter()
                    .map(|st| humanize_type(db, st, RenderLevel::Brief))
                    .collect();
                format!(" : {}", super_names.join(", "))
            }
            _ => String::new(),
        };
        // At Detailed level, humanize_type includes member bodies; insert
        // supers between the class name and the opening brace.
        let humanize_text = humanize_type(
            db,
            &LuaType::Def(type_decl_id.clone()),
            builder.detail_render_level,
        );
        // If humanize_text starts with the class name (e.g. "paper {…}"),
        // splice supers right after it. Otherwise fall back to appending.
        let type_description = if !supers_str.is_empty() && humanize_text.starts_with(class_name) {
            format!(
                "(class) {}{}{}",
                class_name,
                supers_str,
                &humanize_text[class_name.len()..]
            )
        } else {
            format!("(class) {}{}", humanize_text, supers_str)
        };
        type_description
    };

    builder.set_type_description(type_description);
    builder.add_description(&LuaSemanticDeclId::TypeDecl(type_decl_id));
    Some(())
}

fn build_attribute(db: &DbIndex, attribute_name: &str, attribute_type: Option<&LuaType>) -> String {
    let Some(LuaType::DocAttribute(attribute)) = attribute_type else {
        return format!("(attribute) {}", attribute_name);
    };
    let params = attribute
        .get_params()
        .iter()
        .map(|(name, typ)| match typ {
            Some(typ) => {
                let type_name = humanize_type(db, typ, RenderLevel::Normal);
                format!("{}: {}", name, type_name)
            }
            None => name.to_string(),
        })
        .collect::<Vec<_>>();

    if params.is_empty() {
        format!("(attribute) {}", attribute_name)
    } else {
        format!("(attribute) {}({})", attribute_name, params.join(", "))
    }
}
