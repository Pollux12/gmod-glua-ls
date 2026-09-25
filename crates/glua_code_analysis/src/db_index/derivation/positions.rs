//! Edits move the positions that facts are keyed and valued by.
//!
//! Re-indexing a file rebuilds its facts at their new positions. A fact of
//! another file read one of them under its old position: [`EditMaps::map_key`]
//! names the fact it read in new coordinates, and [`fingerprint_before_edits`]
//! fingerprints the current value with the edited files' identities mapped
//! back to their old positions, so a reader whose input only moved compares
//! equal and is not re-run.

use std::cell::{Cell, RefCell};
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicBool, Ordering};

use rowan::{TextRange, TextSize};
use rustc_hash::FxHashMap;

use super::{FactKey, Fingerprint};
use crate::{
    DynamicFieldOwner, FileId, InFiled, LuaDeclId, LuaDefinitionId, LuaMemberId, LuaMemberOwner,
    LuaOperatorOwner, LuaSemanticDeclId, LuaSignatureId, LuaTypeOwner, PositionMap,
};
use glua_parser::LuaSyntaxId;

/// The position maps of the files one update re-indexed.
#[derive(Debug, Default, Clone)]
pub struct EditMaps {
    maps: FxHashMap<FileId, PositionMap>,
}

impl EditMaps {
    pub fn new(maps: impl IntoIterator<Item = (FileId, PositionMap)>) -> Self {
        Self {
            maps: maps
                .into_iter()
                .filter(|(_, map)| !map.is_identity())
                .collect(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.maps.is_empty()
    }

    pub fn files(&self) -> impl Iterator<Item = FileId> + '_ {
        self.maps.keys().copied()
    }

    pub fn map(&self, file_id: FileId) -> Option<PositionMap> {
        self.maps.get(&file_id).copied()
    }

    fn inverse(&self) -> Self {
        Self {
            maps: self
                .maps
                .iter()
                .map(|(file_id, map)| (*file_id, map.inverse()))
                .collect(),
        }
    }

    fn position(&self, file_id: FileId, position: TextSize) -> Option<TextSize> {
        match self.maps.get(&file_id) {
            Some(map) => map.map(position),
            None => Some(position),
        }
    }

    fn range(&self, file_id: FileId, range: TextRange) -> Option<TextRange> {
        match self.maps.get(&file_id) {
            Some(map) => map.map_range(range),
            None => Some(range),
        }
    }

    fn syntax_id(&self, file_id: FileId, id: LuaSyntaxId) -> Option<LuaSyntaxId> {
        Some(id.with_range(self.range(file_id, id.get_range())?))
    }

    fn decl_id(&self, id: LuaDeclId) -> Option<LuaDeclId> {
        Some(LuaDeclId::new(
            id.file_id,
            self.position(id.file_id, id.position)?,
        ))
    }

    fn member_id(&self, id: LuaMemberId) -> Option<LuaMemberId> {
        Some(LuaMemberId::new(
            self.syntax_id(id.file_id, *id.get_syntax_id())?,
            id.file_id,
        ))
    }

    fn signature_id(&self, id: LuaSignatureId) -> Option<LuaSignatureId> {
        Some(LuaSignatureId::new(
            id.get_file_id(),
            self.position(id.get_file_id(), id.get_position())?,
        ))
    }

    fn site(&self, site: &InFiled<TextRange>) -> Option<InFiled<TextRange>> {
        Some(InFiled::new(
            site.file_id,
            self.range(site.file_id, site.value)?,
        ))
    }

    fn definition_id(&self, id: &LuaDefinitionId) -> Option<LuaDefinitionId> {
        Some(match id {
            LuaDefinitionId::Declaration(decl_id) => {
                LuaDefinitionId::Declaration(self.decl_id(*decl_id)?)
            }
            LuaDefinitionId::Assignment {
                file_id,
                assignment,
                target_idx,
            } => LuaDefinitionId::Assignment {
                file_id: *file_id,
                assignment: self.syntax_id(*file_id, *assignment)?,
                target_idx: *target_idx,
            },
        })
    }

    fn member_owner(&self, owner: &LuaMemberOwner) -> Option<LuaMemberOwner> {
        Some(match owner {
            LuaMemberOwner::Element(site) => LuaMemberOwner::Element(self.site(site)?),
            other => other.clone(),
        })
    }

    /// `key` in the coordinates after the edits, or `None` when a position it
    /// names fell inside an edited hunk.
    pub fn map_key(&self, key: &FactKey) -> Option<FactKey> {
        use FactKey as K;
        Some(match key {
            K::TypeCache(owner) => K::TypeCache(match owner {
                LuaTypeOwner::Decl(id) => LuaTypeOwner::Decl(self.decl_id(*id)?),
                LuaTypeOwner::Member(id) => LuaTypeOwner::Member(self.member_id(*id)?),
                LuaTypeOwner::SyntaxId(id) => LuaTypeOwner::SyntaxId(InFiled::new(
                    id.file_id,
                    self.syntax_id(id.file_id, id.value)?,
                )),
            }),
            K::DefinitionFact(id) => K::DefinitionFact(self.definition_id(id)?),
            K::DeclClaim(id) => K::DeclClaim(self.decl_id(*id)?),
            K::Decl(id) => K::Decl(self.decl_id(*id)?),
            K::DeclDefault(id) => K::DeclDefault(self.decl_id(*id)?),
            K::Member(id) => K::Member(self.member_id(*id)?),
            K::MemberSlot(owner, key) => K::MemberSlot(self.member_owner(owner)?, key.clone()),
            K::MemberOwnerMap(owner) => K::MemberOwnerMap(self.member_owner(owner)?),
            K::MemberOwnerExprKeys(owner) => K::MemberOwnerExprKeys(self.member_owner(owner)?),
            K::MemberOwnerFields(owner) => K::MemberOwnerFields(self.member_owner(owner)?),
            K::MemberOwnerPresence(owner) => K::MemberOwnerPresence(self.member_owner(owner)?),
            K::DefinitionSite(site) => K::DefinitionSite(self.site(site)?),
            K::Metatable(site) => K::Metatable(self.site(site)?),
            K::TableClass(site) => K::TableClass(self.site(site)?),
            K::Syntax(site) => K::Syntax(self.site(site)?),
            K::Signature(id) => K::Signature(self.signature_id(*id)?),
            K::ValidGuard(id) => K::ValidGuard(self.signature_id(*id)?),
            K::InferredGuard(id) => K::InferredGuard(self.signature_id(*id)?),
            K::Contrib(id, idx) => K::Contrib(self.signature_id(*id)?, *idx),
            K::SpecialCall(file_id, position) => {
                K::SpecialCall(*file_id, self.position(*file_id, *position)?)
            }
            K::DynamicFieldWildcards(DynamicFieldOwner::Table(site)) => {
                K::DynamicFieldWildcards(DynamicFieldOwner::Table(self.site(site)?))
            }
            K::DynamicFieldFinite(DynamicFieldOwner::Table(site)) => {
                K::DynamicFieldFinite(DynamicFieldOwner::Table(self.site(site)?))
            }
            K::DynamicField(DynamicFieldOwner::Table(site), name) => {
                K::DynamicField(DynamicFieldOwner::Table(self.site(site)?), name.clone())
            }
            K::OperatorOwner(LuaOperatorOwner::Table(site)) => {
                K::OperatorOwner(LuaOperatorOwner::Table(self.site(site)?))
            }
            K::Property(id) => K::Property(match id {
                LuaSemanticDeclId::Member(id) => LuaSemanticDeclId::Member(self.member_id(*id)?),
                LuaSemanticDeclId::LuaDecl(id) => LuaSemanticDeclId::LuaDecl(self.decl_id(*id)?),
                LuaSemanticDeclId::Signature(id) => {
                    LuaSemanticDeclId::Signature(self.signature_id(*id)?)
                }
                other => other.clone(),
            }),
            other => other.clone(),
        })
    }
}

/// Whether any thread is fingerprinting with edits mapped back: a cheap
/// guard in front of the thread-local lookup every positional hash makes.
static MAPPING_BACK: AtomicBool = AtomicBool::new(false);

thread_local! {
    static BACK_MAPS: RefCell<Option<EditMaps>> = const { RefCell::new(None) };
    static HASHING: Cell<bool> = const { Cell::new(false) };
    static FINGERPRINTING: Cell<bool> = const { Cell::new(false) };
    static FACT_FILE: Cell<Option<FileId>> = const { Cell::new(None) };
}

/// Runs `f` with `file_id` as the file whose positions bare ranges, offsets
/// and syntax ids name while fingerprinting (see [`hash_local`]).
pub fn in_file<T>(file_id: Option<FileId>, f: impl FnOnce() -> T) -> T {
    let outer = FACT_FILE.with(|current| current.replace(file_id));
    let result = f();
    FACT_FILE.with(|current| current.set(outer));
    result
}

/// Hashes a position that does not carry its file: one of the file whose
/// fact is being fingerprinted ([`in_file`]).
pub fn hash_local<P: FilePosition + ?Sized, H: Hasher>(position: &P, state: &mut H) {
    let file_id = FACT_FILE.with(Cell::get).unwrap_or(FileId::VIRTUAL);
    position.hash_in_file(file_id, state);
}

/// Whether a fact value is being fingerprinted on this thread: types whose
/// ordinary hash is a cheap bucketing summary hash their whole content then.
pub fn fingerprinting() -> bool {
    FINGERPRINTING.with(Cell::get)
}

/// Whether fingerprints on this thread are being taken with edits mapped
/// back. A fingerprint folded incrementally at write time cannot be mapped,
/// so it is recomputed from its entries then.
pub fn mapping_back() -> bool {
    MAPPING_BACK.load(Ordering::Relaxed) && BACK_MAPS.with(|maps| maps.borrow().is_some())
}

/// Runs `fingerprint` with the edited files' identities hashing as their
/// positions before `edits`.
pub fn fingerprint_before_edits(
    edits: &EditMaps,
    fingerprint: impl FnOnce() -> Fingerprint,
) -> Fingerprint {
    if edits.is_empty() {
        return fingerprint();
    }
    BACK_MAPS.with(|maps| *maps.borrow_mut() = Some(edits.inverse()));
    MAPPING_BACK.store(true, Ordering::Relaxed);
    let result = fingerprint();
    BACK_MAPS.with(|maps| *maps.borrow_mut() = None);
    MAPPING_BACK.store(false, Ordering::Relaxed);
    result
}

/// Hashes a fact value; inside [`fingerprint_before_edits`] its positional
/// identities hash as their pre-edit form. Map lookups made while computing
/// the fingerprint happen outside this and hash normally.
pub(super) fn hash_value<T: Hash + ?Sized, H: Hasher>(value: &T, state: &mut H) {
    let outer = FINGERPRINTING.with(|flag| flag.replace(true));
    let mapping =
        MAPPING_BACK.load(Ordering::Relaxed) && BACK_MAPS.with(|maps| maps.borrow().is_some());
    if mapping {
        HASHING.with(|hashing| hashing.set(true));
    }
    value.hash(state);
    if mapping {
        HASHING.with(|hashing| hashing.set(false));
    }
    FINGERPRINTING.with(|flag| flag.set(outer));
}

/// What a positional identity in `file_id` hashes as right now: `None` to
/// hash it as it is.
fn mapped<T>(file_id: FileId, map: impl FnOnce(&EditMaps) -> Option<T>) -> Option<Option<T>> {
    if !MAPPING_BACK.load(Ordering::Relaxed) || !HASHING.with(Cell::get) {
        return None;
    }
    BACK_MAPS.with(|maps| {
        let maps = maps.borrow();
        let maps = maps.as_ref()?;
        maps.maps.contains_key(&file_id).then(|| map(maps))
    })
}

/// Hashes a position of `file_id`, or a lost marker when it has no pre-edit
/// image.
fn hash_mapped<T: Hash, H: Hasher>(
    file_id: FileId,
    value: &T,
    map: impl FnOnce(&EditMaps) -> Option<T>,
    state: &mut H,
) {
    match mapped(file_id, map) {
        None => value.hash(state),
        Some(Some(old)) => old.hash(state),
        Some(None) => u64::MAX.hash(state),
    }
}

impl Hash for LuaSignatureId {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let file_id = self.get_file_id();
        file_id.hash(state);
        hash_mapped(
            file_id,
            &self.get_position(),
            |maps| maps.position(file_id, self.get_position()),
            state,
        );
    }
}

impl Hash for LuaDeclId {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.file_id.hash(state);
        hash_mapped(
            self.file_id,
            &self.position,
            |maps| maps.position(self.file_id, self.position),
            state,
        );
    }
}

impl Hash for LuaMemberId {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.file_id.hash(state);
        let id = *self.get_syntax_id();
        hash_mapped(
            self.file_id,
            &id,
            |maps| maps.syntax_id(self.file_id, id),
            state,
        );
    }
}

impl Hash for LuaDefinitionId {
    fn hash<H: Hasher>(&self, state: &mut H) {
        match self {
            LuaDefinitionId::Declaration(decl_id) => {
                0u8.hash(state);
                decl_id.hash(state);
            }
            LuaDefinitionId::Assignment {
                file_id,
                assignment,
                target_idx,
            } => {
                1u8.hash(state);
                file_id.hash(state);
                hash_mapped(
                    *file_id,
                    assignment,
                    |maps| maps.syntax_id(*file_id, *assignment),
                    state,
                );
                target_idx.hash(state);
            }
        }
    }
}

/// A value that names a position inside the file an [`InFiled`] wraps it in.
pub trait FilePosition {
    fn hash_in_file<H: Hasher>(&self, file_id: FileId, state: &mut H);
}

impl FilePosition for TextRange {
    fn hash_in_file<H: Hasher>(&self, file_id: FileId, state: &mut H) {
        hash_mapped(file_id, self, |maps| maps.range(file_id, *self), state);
    }
}

impl FilePosition for TextSize {
    fn hash_in_file<H: Hasher>(&self, file_id: FileId, state: &mut H) {
        hash_mapped(file_id, self, |maps| maps.position(file_id, *self), state);
    }
}

impl FilePosition for LuaSyntaxId {
    fn hash_in_file<H: Hasher>(&self, file_id: FileId, state: &mut H) {
        hash_mapped(file_id, self, |maps| maps.syntax_id(file_id, *self), state);
    }
}

impl<P: FilePosition> FilePosition for Option<P> {
    fn hash_in_file<H: Hasher>(&self, file_id: FileId, state: &mut H) {
        self.is_some().hash(state);
        if let Some(position) = self {
            position.hash_in_file(file_id, state);
        }
    }
}

impl<P: FilePosition> FilePosition for [P] {
    fn hash_in_file<H: Hasher>(&self, file_id: FileId, state: &mut H) {
        self.len().hash(state);
        for position in self {
            position.hash_in_file(file_id, state);
        }
    }
}

impl<P: FilePosition> FilePosition for Vec<P> {
    fn hash_in_file<H: Hasher>(&self, file_id: FileId, state: &mut H) {
        self.as_slice().hash_in_file(file_id, state);
    }
}

impl<N: FilePosition> Hash for InFiled<N> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.file_id.hash(state);
        self.value.hash_in_file(self.file_id, state);
    }
}
