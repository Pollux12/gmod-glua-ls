use glua_parser::LuaExpr;

use crate::{FileId, InFiled, LuaDeclId, LuaMemberId, LuaSignatureId, LuaTypeDeclId};

#[derive(Debug, PartialEq, Eq, Clone, Hash)]
pub enum InferFailReason {
    None,
    RecursiveInfer,
    UnResolveExpr(InFiled<LuaExpr>),
    UnResolveSignatureReturn(LuaSignatureId),
    FieldNotFound,
    UnResolveDeclType(LuaDeclId),
    UnResolveTypeDecl(LuaTypeDeclId),
    UnResolveMemberType(LuaMemberId),
    UnResolveOperatorCall,
    UnResolveModuleExport(FileId),
    /// The dynamic-field index is still being built, so a read of it would
    /// answer from however far the batch walk happened to get. Clears when the
    /// index seals.
    UnSealedDynamicFields,
    /// A `for ... in` iterator variable holds a raw template ref because nothing
    /// bound the iterator function's generic. The placeholder is already cached,
    /// so this group only ever upgrades it; failures stay inside the group rather
    /// than joining another reason's fixpoint.
    UnResolveIterTemplate,
}

impl InferFailReason {
    pub fn is_need_resolve(&self) -> bool {
        matches!(
            self,
            InferFailReason::UnResolveExpr(_)
                | InferFailReason::UnResolveSignatureReturn(_)
                | InferFailReason::FieldNotFound
                | InferFailReason::UnResolveDeclType(_)
                | InferFailReason::UnResolveTypeDecl(_)
                | InferFailReason::UnResolveMemberType(_)
                | InferFailReason::UnResolveOperatorCall
                | InferFailReason::UnResolveModuleExport(_)
                | InferFailReason::UnSealedDynamicFields
                | InferFailReason::UnResolveIterTemplate
        )
    }
}
