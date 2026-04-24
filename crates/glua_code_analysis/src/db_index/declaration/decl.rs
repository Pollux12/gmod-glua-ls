use crate::FileId;
use crate::{LuaMemberId, LuaSignatureId};
use glua_parser::{LuaKind, LuaSyntaxId, LuaSyntaxKind};
use rowan::{TextRange, TextSize};
use smol_str::SmolStr;

use super::decl_id::LuaDeclId;

#[derive(Eq, PartialEq, Hash, Debug, Clone)]
pub struct LuaDecl {
    name: SmolStr,
    file_id: FileId,
    range: TextRange,
    expr_id: Option<LuaSyntaxId>,
    initializer: Option<LuaDeclInitializer>,
    pub extra: LuaDeclExtra,
    /// True when this declaration was synthetically injected by the language server
    /// (e.g. the scoped-class variable `GM`/`ENT`/`SWEP` seeded into gamemode/entity files).
    /// Such decls are not present in the user's source and must be excluded from
    /// diagnostics that rely on user-written variable declarations (e.g. `unused`, `redefined-local`).
    is_seeded_class_local: bool,
}

#[derive(Eq, PartialEq, Hash, Debug, Clone, Copy)]
pub struct LuaDeclInitializer {
    expr_id: LuaSyntaxId,
    ret_idx: usize,
}

impl LuaDeclInitializer {
    pub fn new(expr_id: LuaSyntaxId, ret_idx: usize) -> Self {
        Self { expr_id, ret_idx }
    }

    pub fn get_expr_syntax_id(&self) -> LuaSyntaxId {
        self.expr_id
    }

    pub fn get_ret_idx(&self) -> usize {
        self.ret_idx
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
            is_seeded_class_local: false,
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

    /// Returns `true` when this declaration was synthetically injected by the LS
    /// (e.g. the scoped-class variable `GM` seeded into gamemode files).
    /// Such decls have no corresponding source text and must be skipped by
    /// diagnostics that inspect user-written variable declarations.
    pub fn is_seeded_class_local(&self) -> bool {
        self.is_seeded_class_local
    }

    pub fn mark_seeded_class_local(&mut self) {
        self.is_seeded_class_local = true;
    }
}

#[derive(Eq, PartialEq, Hash, Debug, Clone)]
pub enum LocalAttribute {
    Const,
    Close,
    IterConst,
}
