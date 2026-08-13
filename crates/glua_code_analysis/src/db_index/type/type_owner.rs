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

    /// Whether a write of `self` should displace `existing`.
    pub fn supersedes(&self, existing: &LuaTypeCache) -> bool {
        // A real type always beats a "no value found" placeholder. Declared
        // types may do this too — an annotation outranks a stale inference.
        // `any`/`unknown` are authoritative statements that a value is
        // unconstrained, so they do not count as informative here.
        if matches!(existing, LuaTypeCache::InferType(typ) if is_bottom_type(typ))
            && is_informative_type(self.as_type())
        {
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
        if let (Some(existing_key), Some(new_key)) = (
            uninformative_key(existing_type),
            uninformative_key(new_type),
        ) {
            return new_key > existing_key;
        }

        // A literal and its widened primitive describe the same value at
        // different precision. Keeping the widened form is the join, and stops
        // the stored type depending on whether the widening pass or the literal
        // inference reached the owner first.
        widens_primitive(new_type, existing_type)
    }
}

const NIL_RANK: u8 = 1;

/// Rank within the "carries no type information" band, ordered by how much
/// the value could be: `never` (nothing) through `any` (anything). `None`
/// means the type carries information, i.e.
pub(crate) fn uninformative_rank(typ: &LuaType) -> Option<u8> {
    match typ {
        LuaType::Never => Some(0),
        LuaType::Nil => Some(NIL_RANK),
        LuaType::Unknown => Some(2),
        LuaType::Any => Some(3),
        LuaType::Union(union) => union
            .types()
            .try_fold(0, |rank: u8, typ| Some(rank.max(uninformative_rank(typ)?))),
        LuaType::MultiLineUnion(union) => {
            union.get_unions().iter().try_fold(0, |rank: u8, (typ, _)| {
                Some(rank.max(uninformative_rank(typ)?))
            })
        }
        _ => None,
    }
}

/// Total order over the "carries no type information" band:
/// [`uninformative_rank`] first, then the nullable variant ahead of the
/// bare one.
fn uninformative_key(typ: &LuaType) -> Option<(u8, u8)> {
    Some((uninformative_rank(typ)?, typ.is_nullable() as u8))
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

/// The bottom of the lattice — `nil`/`never`, or a union of only those. Records
/// "no value was found" rather than "any value is allowed".
pub(crate) fn is_bottom_type(typ: &LuaType) -> bool {
    uninformative_rank(typ).is_some_and(|rank| rank <= NIL_RANK)
}

/// Whether `typ` says anything about the value. The single authoritative
/// definition of "informative"; everything else derives from
/// [`uninformative_rank`].
pub fn is_informative_type(typ: &LuaType) -> bool {
    uninformative_rank(typ).is_none()
}

impl std::ops::Deref for LuaTypeCache {
    type Target = LuaType;

    fn deref(&self) -> &Self::Target {
        self.as_type()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LuaUnionType;

    fn union(types: Vec<LuaType>) -> LuaType {
        LuaType::from_vec(types)
    }

    #[test]
    fn union_of_uninformative_members_ranks_with_its_widest_member() {
        // `any|nil` says exactly as little as `any`. Ranking it `None` let a
        // deferred round freeze a local against every later real inference.
        assert_eq!(
            uninformative_rank(&union(vec![LuaType::Any, LuaType::Nil])),
            Some(3)
        );
        assert_eq!(
            uninformative_rank(&union(vec![LuaType::Any, LuaType::Nil, LuaType::Never])),
            Some(3)
        );
        assert!(!is_informative_type(&union(vec![
            LuaType::Any,
            LuaType::Nil
        ])));

        // One informative member makes the whole union informative.
        assert_eq!(
            uninformative_rank(&union(vec![LuaType::String, LuaType::Nil])),
            None
        );
        assert!(is_informative_type(&union(vec![
            LuaType::String,
            LuaType::Nil
        ])));

        // `any|nil` is not bottom: it admits every value, not none.
        assert!(!is_bottom_type(&union(vec![LuaType::Any, LuaType::Nil])));
        assert!(is_bottom_type(&union(vec![LuaType::Nil, LuaType::Never])));
    }

    /// Every ordered pair inside the uninformative band must get exactly one
    /// verdict, so the slot converges on the same join no matter which write
    /// lands first. Rank alone tied `unknown` with `unknown|nil` (and `any`
    /// with `any|nil`), which left write order deciding whether the slot was
    /// sticky or displaceable.
    #[test]
    fn supersedes_is_a_total_order_inside_the_uninformative_band() {
        // `unknown|nil` survives construction now that an `unknown` arm is kept,
        // so it is a real member of the band and ties with bare `unknown` on
        // rank — exactly the tie `uninformative_key` breaks with `is_nullable`.
        assert_eq!(
            union(vec![LuaType::Unknown, LuaType::Nil]),
            LuaType::from(LuaUnionType::Nullable(LuaType::Unknown))
        );

        let band = [
            ("never", LuaType::Never),
            ("nil", LuaType::Nil),
            ("unknown", LuaType::Unknown),
            ("unknown|nil", union(vec![LuaType::Unknown, LuaType::Nil])),
            ("any", LuaType::Any),
            ("any|nil", union(vec![LuaType::Any, LuaType::Nil])),
        ];

        for (i, (a_name, a)) in band.iter().enumerate() {
            let a_cache = LuaTypeCache::InferType(a.clone());
            assert!(
                !a_cache.supersedes(&a_cache),
                "{a_name} superseded itself, so repeating a write is not a no-op"
            );

            for (b_name, b) in band.iter().skip(i + 1) {
                let b_cache = LuaTypeCache::InferType(b.clone());
                // Listed widest-last, so the later entry always wins.
                assert!(
                    b_cache.supersedes(&a_cache),
                    "{b_name} must supersede {a_name}"
                );
                assert!(
                    !a_cache.supersedes(&b_cache),
                    "{a_name} must not supersede {b_name}"
                );
            }
        }
    }
}
