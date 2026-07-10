#[cfg(test)]
mod test {
    use crate::{Emmyrc, VirtualWorkspace};
    use glua_parser::{LuaAstNode, LuaNameExpr};

    fn enable_gmod(ws: &mut VirtualWorkspace) {
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
    }

    fn last_name_type(ws: &VirtualWorkspace, file_id: crate::FileId, name: &str) -> crate::LuaType {
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("semantic model");
        let name_expr = semantic_model
            .get_root()
            .descendants::<LuaNameExpr>()
            .filter(|expr| expr.get_name_text().as_deref() == Some(name))
            .last()
            .expect("name expression");
        semantic_model
            .get_semantic_info(name_expr.syntax().clone().into())
            .expect("semantic info")
            .display_typ()
            .clone()
    }

    #[test]
    fn unguarded_child_member_evidence_selects_player_and_resolves_member_result() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);

        let file_id = ws.def(
            r#"
            ---@class Vector

            ---@class Entity
            ---@class Player: Entity
            ---@field GetShootPos fun(self: Player): Vector
            ---@field IsActiveCannibal fun(self: Player): boolean
            ---@field IsRoleAbilityDisabled fun(self: Player): boolean

            ---@type Entity
            local owner
            local spos = owner:GetShootPos()
            local active = owner:IsActiveCannibal()
            local disabled = owner:IsRoleAbilityDisabled()
            print(owner)
            print(spos)
            "#,
        );

        let owner_type = last_name_type(&ws, file_id, "owner");
        let spos_type = last_name_type(&ws, file_id, "spos");
        assert_eq!(ws.humanize_type(owner_type), "Player");
        assert_eq!(ws.humanize_type(spos_type), "Vector");
    }
}
