use glua_parser::LuaSyntaxId;
use rowan::TextSize;

use crate::{FileId, InFiled, LuaDeclId, LuaMemberId};

use super::LuaType;

#[derive(Debug, Eq, PartialEq, Hash, Clone)]
pub enum LuaTypeOwner {
    Decl(LuaDeclId),
    Member(LuaMemberId),
    SyntaxId(InFiled<LuaSyntaxId>),
}

impl From<LuaDeclId> for LuaTypeOwner {
    fn from(decl_id: LuaDeclId) -> Self {
        Self::Decl(decl_id)
    }
}

impl From<LuaMemberId> for LuaTypeOwner {
    fn from(member_id: LuaMemberId) -> Self {
        Self::Member(member_id)
    }
}

impl From<InFiled<LuaSyntaxId>> for LuaTypeOwner {
    fn from(syntax_id: InFiled<LuaSyntaxId>) -> Self {
        Self::SyntaxId(syntax_id)
    }
}

impl LuaTypeOwner {
    pub fn get_file_id(&self) -> FileId {
        match self {
            LuaTypeOwner::Decl(id) => id.file_id,
            LuaTypeOwner::Member(id) => id.file_id,
            LuaTypeOwner::SyntaxId(id) => id.file_id,
        }
    }

    pub fn get_position(&self) -> TextSize {
        match self {
            LuaTypeOwner::Decl(id) => id.position,
            LuaTypeOwner::Member(id) => id.get_position(),
            LuaTypeOwner::SyntaxId(id) => id.value.get_range().start(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum LuaTypeCache {
    DocType(LuaType),
    InferType(LuaType),
}

impl LuaTypeCache {
    pub fn as_type(&self) -> &LuaType {
        match self {
            LuaTypeCache::DocType(ty) => ty,
            LuaTypeCache::InferType(ty) => ty,
        }
    }

    pub fn is_infer(&self) -> bool {
        matches!(self, LuaTypeCache::InferType(_))
    }

    pub fn is_doc(&self) -> bool {
        matches!(self, LuaTypeCache::DocType(_))
    }

    /// Whether this cache records "no value was found" rather than a type.
    pub fn is_bottom_infer(&self) -> bool {
        match self {
            LuaTypeCache::DocType(_) => false,
            LuaTypeCache::InferType(typ) => is_bottom_type(typ),
        }
    }

    /// Whether this cache carries usable type information — the counterpart of
    /// [`is_bottom_infer`](Self::is_bottom_infer) at the other end of the
    /// lattice. `any`/`unknown` are authoritative statements that a value is
    /// unconstrained, so they are not "informative" for replacement purposes.
    pub fn is_informative(&self) -> bool {
        is_informative_type(self.as_type())
    }

    /// Whether a write of `self` should displace `existing`.
    pub fn supersedes(&self, existing: &LuaTypeCache) -> bool {
        // A real type always beats a "no value found" placeholder. Declared
        // types may do this too — an annotation outranks a stale inference.
        if existing.is_bottom_infer() && self.is_informative() {
            return true;
        }

        let (LuaTypeCache::InferType(existing_type), LuaTypeCache::InferType(new_type)) =
            (existing, self)
        else {
            return false;
        };

        // Two answers that both carry no type information: keep the one that
        // admits more values. Racing `nil` against `unknown` (or `never`
        // against `any`) otherwise left the owner holding whichever round
        // happened to arrive first, and `nil` vs `unknown` is the difference
        // between reporting a nil-check and not.
        if let (Some(existing_rank), Some(new_rank)) = (
            uninformative_rank(existing_type),
            uninformative_rank(new_type),
        ) {
            return new_rank > existing_rank;
        }

        // A literal and its widened primitive describe the same value at
        // different precision. Keeping the widened form is the join, and stops
        // the stored type depending on whether the widening pass or the literal
        // inference reached the owner first.
        widens_primitive(new_type, existing_type)
    }
}

/// Rank within the "carries no type information" band, ordered by how much the
/// value could be: `never` (nothing) through `any` (anything).
fn uninformative_rank(typ: &LuaType) -> Option<u8> {
    match typ {
        LuaType::Never => Some(0),
        LuaType::Nil => Some(1),
        LuaType::Unknown => Some(2),
        LuaType::Any => Some(3),
        _ => None,
    }
}

/// Whether `wider` is the widened primitive of the literal `narrower`.
fn widens_primitive(wider: &LuaType, narrower: &LuaType) -> bool {
    matches!(
        (wider, narrower),
        (
            LuaType::String,
            LuaType::StringConst(_) | LuaType::DocStringConst(_)
        ) | (
            LuaType::Integer,
            LuaType::IntegerConst(_) | LuaType::DocIntegerConst(_)
        ) | (
            LuaType::Number,
            LuaType::FloatConst(_) | LuaType::IntegerConst(_) | LuaType::DocIntegerConst(_)
        ) | (
            LuaType::Boolean,
            LuaType::BooleanConst(_) | LuaType::DocBooleanConst(_)
        )
    )
}

pub(crate) fn is_bottom_type(typ: &LuaType) -> bool {
    match typ {
        LuaType::Nil | LuaType::Never => true,
        LuaType::Union(union) => union.types().all(is_bottom_type),
        LuaType::MultiLineUnion(union) => union
            .get_unions()
            .iter()
            .all(|(typ, _)| is_bottom_type(typ)),
        _ => false,
    }
}

pub fn is_informative_type(typ: &LuaType) -> bool {
    match typ {
        LuaType::Any | LuaType::Unknown | LuaType::Nil | LuaType::Never => false,
        LuaType::Union(union) => union.types().any(is_informative_type),
        LuaType::MultiLineUnion(union) => union
            .get_unions()
            .iter()
            .any(|(typ, _)| is_informative_type(typ)),
        _ => true,
    }
}

impl std::ops::Deref for LuaTypeCache {
    type Target = LuaType;

    fn deref(&self) -> &Self::Target {
        self.as_type()
    }
}
