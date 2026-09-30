//! How a statement's analysis reaches the index: inline, writing as it goes,
//! or deferred, reading a shared index and logging its writes for the engine
//! to apply afterwards.

use std::ops::{Deref, DerefMut};

use crate::{
    DbIndex, LuaDeclId, LuaInferenceNodeId, LuaMember, LuaMemberOwner, LuaTypeCache, LuaTypeFact,
    LuaTypeOwner,
    compilation::analyzer::common::{
        DeclWrite, TypeCacheWriteMode, bind_decl_write, bind_type, write_type_cache,
    },
};

/// The index as a statement's analysis sees it.
pub(in crate::compilation::analyzer) enum DbAccess<'a> {
    /// Writes apply as they are made.
    Inline(&'a mut DbIndex),
    /// Writes are logged and apply after the analysis; a write that bypasses
    /// the log panics.
    Deferred(&'a DbIndex),
}

impl Deref for DbAccess<'_> {
    type Target = DbIndex;

    fn deref(&self) -> &DbIndex {
        match self {
            DbAccess::Inline(db) => db,
            DbAccess::Deferred(db) => db,
        }
    }
}

impl DerefMut for DbAccess<'_> {
    fn deref_mut(&mut self) -> &mut DbIndex {
        match self {
            DbAccess::Inline(db) => db,
            DbAccess::Deferred(_) => {
                unreachable!("a deferred statement analysis writes only through its write log")
            }
        }
    }
}

impl std::fmt::Debug for DbAccess<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            DbAccess::Inline(_) => "Inline",
            DbAccess::Deferred(_) => "Deferred",
        })
    }
}

/// One index write a statement's analysis made, applied through the same
/// write path an inline analysis calls, against the index as it stands when
/// it applies.
#[derive(Debug)]
pub(in crate::compilation::analyzer) enum StatementWrite {
    AddMember {
        owner: LuaMemberOwner,
        member: LuaMember,
    },
    BindType {
        owner: LuaTypeOwner,
        cache: LuaTypeCache,
    },
    WriteTypeCache {
        owner: LuaTypeOwner,
        cache: LuaTypeCache,
        mode: TypeCacheWriteMode,
    },
    BindDeclWrite {
        decl_id: LuaDeclId,
        cache: LuaTypeCache,
        write: DeclWrite,
    },
    /// Publishes an initializer's inference fact for its declaration when the
    /// declaration's stored type is the fact's type.
    PublishInitializerFact {
        decl_id: LuaDeclId,
        fact: LuaTypeFact,
    },
}

impl StatementWrite {
    pub(in crate::compilation::analyzer) fn apply(&self, db: &mut DbIndex) {
        match self {
            StatementWrite::AddMember { owner, member } => {
                db.get_member_index_mut()
                    .add_member(owner.clone(), member.clone());
            }
            StatementWrite::BindType { owner, cache } => {
                bind_type(db, owner.clone(), cache.clone());
            }
            StatementWrite::WriteTypeCache { owner, cache, mode } => {
                write_type_cache(db, owner.clone(), cache.clone(), *mode);
            }
            StatementWrite::BindDeclWrite {
                decl_id,
                cache,
                write,
            } => {
                bind_decl_write(db, *decl_id, cache.clone(), *write);
            }
            StatementWrite::PublishInitializerFact { decl_id, fact } => {
                let owner = LuaTypeOwner::Decl(*decl_id);
                if db
                    .get_type_index()
                    .get_type_cache(&owner)
                    .is_some_and(|cache| cache.as_type() == fact.typ())
                {
                    db.publish_inference_facts(vec![(
                        LuaInferenceNodeId::TypeOwner(owner),
                        fact.clone(),
                    )]);
                }
            }
        }
    }
}
