mod intersect_type;
mod remove_type;
mod test;
pub(crate) mod union_type;

use super::LuaType;
use crate::DbIndex;

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub enum TypeOps {
    /// Add a type to the source type
    Union,
    /// Intersect a type with the source type
    Intersect,
    /// Remove a type from the source type
    Remove,
}

impl TypeOps {
    pub fn apply(&self, db: &DbIndex, source: &LuaType, target: &LuaType) -> LuaType {
        match self {
            TypeOps::Union => union_type::union_type(db, source.clone(), target.clone()),
            TypeOps::Intersect => {
                intersect_type::intersect_type(db, source.clone(), target.clone())
            }
            TypeOps::Remove => {
                let result = remove_type::remove_type(db, source.clone(), target.clone());
                if let Some(result) = result {
                    return result;
                }

                match &source {
                    LuaType::Nil => LuaType::Never,
                    _ => source.clone(),
                }
            }
        }
    }

    /// Batch equivalent of folding [`TypeOps::Union`] from [`LuaType::Never`].
    ///
    /// Collect loop outputs and finalize once instead of unioning pairwise.
    /// The answer is exactly the ordered fold for every input; a structural
    /// fast path is taken only when proven equivalent, otherwise the fold
    /// itself runs.
    pub fn union_all(db: &DbIndex, types: Vec<LuaType>) -> LuaType {
        union_type::union_all(db, types)
    }
}
