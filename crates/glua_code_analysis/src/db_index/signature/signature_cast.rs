use glua_parser::{BinaryOperator, LuaDocOpType};

use crate::LuaType;

/// Whether a cast adds its target to a type, removes it, or replaces the
/// type with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LuaCastAction {
    Add,
    Remove,
    Force,
}

impl LuaCastAction {
    pub fn negated(self) -> Self {
        match self {
            Self::Add | Self::Force => Self::Remove,
            Self::Remove => Self::Add,
        }
    }
}

/// What a cast adds, removes or forces.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum LuaCastTarget {
    /// `+?` / `-?`.
    Nil,
    Type(LuaType),
}

/// One `---@cast` or `---@return_cast` operation with its type resolved.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LuaCastOp {
    pub action: LuaCastAction,
    pub target: Option<LuaCastTarget>,
}

impl LuaCastOp {
    /// The operation `op_type` writes, given its doc type's resolved type.
    pub fn new(op_type: &LuaDocOpType, typ: Option<LuaType>) -> Self {
        let action = match op_type.get_op() {
            Some(op) if op.get_op() == BinaryOperator::OpAdd => LuaCastAction::Add,
            Some(_) => LuaCastAction::Remove,
            None => LuaCastAction::Force,
        };
        let target = if op_type.is_nullable() {
            Some(LuaCastTarget::Nil)
        } else {
            typ.map(LuaCastTarget::Type)
        };
        Self { action, target }
    }
}

/// A signature's `---@return_cast`: the parameter it narrows, the cast when
/// the call returns truthy, and an optional cast for the falsy branch.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LuaSignatureCast {
    pub name: String,
    pub cast: LuaCastOp,
    pub fallback_cast: Option<LuaCastOp>,
}
