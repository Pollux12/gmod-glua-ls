use std::hash::{Hash, Hasher};

use crate::FileId;
use crate::db_index::derivation::FilePosition;
use crate::{LuaMemberId, LuaSignatureId};
use glua_parser::{LuaAstNode, LuaKind, LuaSyntaxId, LuaSyntaxKind, LuaTableExpr};
use rowan::{TextRange, TextSize};
use smol_str::SmolStr;

use super::decl_id::LuaDeclId;

#[derive(Eq, PartialEq, Debug, Clone)]
pub struct LuaDecl {
    name: SmolStr,
    file_id: FileId,
    range: TextRange,
    expr_id: Option<LuaSyntaxId>,
    initializer: Option<LuaDeclInitializer>,
    pub extra: LuaDeclExtra,
}

impl Hash for LuaDecl {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.name.hash(state);
        self.file_id.hash(state);
        self.range.hash_in_file(self.file_id, state);
        self.expr_id.is_some().hash(state);
        if let Some(expr_id) = &self.expr_id {
            expr_id.hash_in_file(self.file_id, state);
        }
        self.initializer.is_some().hash(state);
        if let Some(initializer) = &self.initializer {
            initializer.expr_id.hash_in_file(self.file_id, state);
            (initializer.ret_idx as usize).hash(state);
        }
        self.extra.hash(state);
    }
}

#[derive(Eq, PartialEq, Hash, Debug, Clone, Copy)]
pub struct LuaDeclInitializer {
    expr_id: LuaSyntaxId,
    ret_idx: u32,
}

impl LuaDeclInitializer {
    pub fn new(expr_id: LuaSyntaxId, ret_idx: usize) -> Self {
        Self {
            expr_id,
            ret_idx: ret_idx as u32,
        }
    }

    pub fn get_expr_syntax_id(&self) -> LuaSyntaxId {
        self.expr_id
    }

    pub fn get_ret_idx(&self) -> usize {
        self.ret_idx as usize
    }
}

#[derive(Eq, PartialEq, Hash, Debug, Clone)]
pub enum LuaDeclExtra {
    Local {
        kind: LuaKind,
        attrib: Option<LocalAttribute>,
    },
    Param {
        idx: usize,
        signature_id: LuaSignatureId,
        owner_member_id: Option<LuaMemberId>,
    },
    ImplicitSelf {
        kind: LuaKind,
    },
    Global {
        kind: LuaKind,
    },
    Module {
        kind: LuaKind,
        module_path: SmolStr,
    },
}

impl LuaDecl {
    pub fn new(
        name: &str,
        file_id: FileId,
        range: TextRange,
        extra: LuaDeclExtra,
        expr_id: Option<LuaSyntaxId>,
    ) -> Self {
        Self {
            name: SmolStr::new(name),
            file_id,
            range,
            expr_id,
            initializer: expr_id.map(|expr_id| LuaDeclInitializer::new(expr_id, 0)),
            extra,
        }
    }

    pub fn get_file_id(&self) -> FileId {
        self.file_id
    }

    pub fn get_id(&self) -> LuaDeclId {
        LuaDeclId::new(self.file_id, self.range.start())
    }

    pub fn get_name(&self) -> &str {
        &self.name
    }

    pub fn get_position(&self) -> TextSize {
        self.range.start()
    }

    pub fn get_range(&self) -> TextRange {
        self.range
    }

    pub fn get_syntax_id(&self) -> LuaSyntaxId {
        match self.extra {
            LuaDeclExtra::Local { kind, .. } => LuaSyntaxId::new(kind, self.range),
            LuaDeclExtra::Param { .. } => {
                LuaSyntaxId::new(LuaSyntaxKind::ParamName.into(), self.range)
            }
            LuaDeclExtra::ImplicitSelf { kind } => LuaSyntaxId::new(kind, self.range),
            LuaDeclExtra::Global { kind, .. } | LuaDeclExtra::Module { kind, .. } => {
                LuaSyntaxId::new(kind, self.range)
            }
        }
    }

    pub fn get_value_syntax_id(&self) -> Option<LuaSyntaxId> {
        self.expr_id
    }

    pub fn get_initializer(&self) -> Option<LuaDeclInitializer> {
        self.initializer
    }

    pub fn has_initializer(&self) -> bool {
        self.initializer.is_some()
    }

    /// The table literal the declaration takes as its first value, if any.
    pub fn table_literal_initializer(&self) -> Option<LuaSyntaxId> {
        let initializer = self.initializer?;
        (initializer.ret_idx == 0 && LuaTableExpr::can_cast(initializer.expr_id.get_kind()))
            .then_some(initializer.expr_id)
    }

    pub fn set_initializer(&mut self, initializer: Option<LuaDeclInitializer>) {
        self.initializer = initializer;
    }

    pub fn is_local(&self) -> bool {
        matches!(
            &self.extra,
            LuaDeclExtra::Local { .. }
                | LuaDeclExtra::Param { .. }
                | LuaDeclExtra::ImplicitSelf { .. }
        )
    }

    pub fn is_param(&self) -> bool {
        matches!(&self.extra, LuaDeclExtra::Param { .. })
    }

    pub fn is_global(&self) -> bool {
        matches!(&self.extra, LuaDeclExtra::Global { .. })
    }

    pub fn is_module_scoped(&self) -> bool {
        matches!(&self.extra, LuaDeclExtra::Module { .. })
    }

    pub fn get_module_path(&self) -> Option<&str> {
        match &self.extra {
            LuaDeclExtra::Module { module_path, .. } => Some(module_path.as_str()),
            _ => None,
        }
    }

    pub fn is_implicit_self(&self) -> bool {
        matches!(&self.extra, LuaDeclExtra::ImplicitSelf { .. })
    }

    /// Whether a macro call such as `DEFINE_BASECLASS("X")` declares this local at its arguments.
    pub fn is_macro_local(&self) -> bool {
        matches!(&self.extra, LuaDeclExtra::Local { kind, .. } if *kind == LuaSyntaxKind::CallArgList.into())
    }
}

#[derive(Eq, PartialEq, Hash, Debug, Clone)]
pub enum LocalAttribute {
    Const,
    Close,
    IterConst,
}
