#[derive(Debug, Eq, PartialEq, Hash, Clone, Copy)]
pub enum LuaOperatorMetaMethod {
    Add,    // +
    Sub,    // -
    Mul,    // *
    Div,    // /
    Mod,    // %
    Pow,    // ^
    Unm,    // -
    IDiv,   // //
    BAnd,   // &
    BOr,    // |
    BXor,   // ~
    BNot,   // ~
    Shl,    // <<
    Shr,    // >>
    Concat, // ..
    Len,    // #
    Eq,     // ==
    Lt,     // <
    Le,     // <=
    Index,  // __index
    Call,   // __call
    Pairs,  // __pairs unimplemented
}

impl LuaOperatorMetaMethod {
    pub fn from_operator_name(op: &str) -> Option<Self> {
        match op {
            "add" => Some(LuaOperatorMetaMethod::Add),
            "sub" => Some(LuaOperatorMetaMethod::Sub),
            "mul" => Some(LuaOperatorMetaMethod::Mul),
            "div" => Some(LuaOperatorMetaMethod::Div),
            "mod" => Some(LuaOperatorMetaMethod::Mod),
            "pow" => Some(LuaOperatorMetaMethod::Pow),
            "unm" => Some(LuaOperatorMetaMethod::Unm),
            "idiv" => Some(LuaOperatorMetaMethod::IDiv),
            "band" => Some(LuaOperatorMetaMethod::BAnd),
            "bor" => Some(LuaOperatorMetaMethod::BOr),
            "bxor" => Some(LuaOperatorMetaMethod::BXor),
            "bnot" => Some(LuaOperatorMetaMethod::BNot),
            "shl" => Some(LuaOperatorMetaMethod::Shl),
            "shr" => Some(LuaOperatorMetaMethod::Shr),
            "concat" => Some(LuaOperatorMetaMethod::Concat),
            "len" => Some(LuaOperatorMetaMethod::Len),
            "eq" => Some(LuaOperatorMetaMethod::Eq),
            "lt" => Some(LuaOperatorMetaMethod::Lt),
            "le" => Some(LuaOperatorMetaMethod::Le),
            "call" => Some(LuaOperatorMetaMethod::Call),
            "pairs" => Some(LuaOperatorMetaMethod::Pairs),
            _ => None,
        }
    }

    pub fn from_metatable_name(name: &str) -> Option<Self> {
        METATABLE_NAMES
            .iter()
            .find(|(metatable_name, _)| *metatable_name == name)
            .map(|(_, method)| *method)
    }

    /// The metatable key Lua reads for this metamethod, such as `__add`.
    pub fn metatable_name(self) -> Option<&'static str> {
        METATABLE_NAMES
            .iter()
            .find(|(_, method)| *method == self)
            .map(|(name, _)| *name)
    }
}

const METATABLE_NAMES: [(&str, LuaOperatorMetaMethod); 21] = [
    ("__add", LuaOperatorMetaMethod::Add),
    ("__sub", LuaOperatorMetaMethod::Sub),
    ("__mul", LuaOperatorMetaMethod::Mul),
    ("__div", LuaOperatorMetaMethod::Div),
    ("__mod", LuaOperatorMetaMethod::Mod),
    ("__pow", LuaOperatorMetaMethod::Pow),
    ("__unm", LuaOperatorMetaMethod::Unm),
    ("__idiv", LuaOperatorMetaMethod::IDiv),
    ("__band", LuaOperatorMetaMethod::BAnd),
    ("__bor", LuaOperatorMetaMethod::BOr),
    ("__bxor", LuaOperatorMetaMethod::BXor),
    ("__bnot", LuaOperatorMetaMethod::BNot),
    ("__shl", LuaOperatorMetaMethod::Shl),
    ("__shr", LuaOperatorMetaMethod::Shr),
    ("__concat", LuaOperatorMetaMethod::Concat),
    ("__len", LuaOperatorMetaMethod::Len),
    ("__eq", LuaOperatorMetaMethod::Eq),
    ("__lt", LuaOperatorMetaMethod::Lt),
    ("__le", LuaOperatorMetaMethod::Le),
    ("__index", LuaOperatorMetaMethod::Index),
    ("__call", LuaOperatorMetaMethod::Call),
];
