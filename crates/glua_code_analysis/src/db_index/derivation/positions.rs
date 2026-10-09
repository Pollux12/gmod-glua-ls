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
use std::sync::atomic::{AtomicUsize, Ordering};

use rowan::{TextRange, TextSize};
use rustc_hash::FxHashMap;

use super::{FactKey, Fingerprint};
use crate::{
    DynamicFieldOwner, FileId, InFiled, LuaDeclId, LuaDeclOrMemberId, LuaDefinitionId, LuaMemberId,
    LuaMemberKey, LuaMemberOwner, LuaOperatorOwner, LuaSemanticDeclId, LuaSignatureId,
    LuaTypeOwner, PositionMap, SelfRefId, VarRefId, VarRefRootId,
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

    /// Whether these edits change `file_id`'s text.
    pub fn edits(&self, file_id: FileId) -> bool {
        self.maps.contains_key(&file_id)
    }

    pub fn map(&self, file_id: FileId) -> Option<PositionMap> {
        self.maps.get(&file_id).cloned()
    }

    /// These edits followed by `later`, edits of the text these produced.
    pub fn then(&self, later: &Self) -> Self {
        let mut maps = self.maps.clone();
        for (file_id, map) in &later.maps {
            let composed = maps
                .get(file_id)
                .map_or_else(|| map.clone(), |earlier| earlier.then(map));
            maps.insert(*file_id, composed);
        }
        Self::new(maps)
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

    fn decl_or_member_id(&self, id: &LuaDeclOrMemberId) -> Option<LuaDeclOrMemberId> {
        Some(match id {
            LuaDeclOrMemberId::Decl(id) => LuaDeclOrMemberId::Decl(self.decl_id(*id)?),
            LuaDeclOrMemberId::Member(id) => LuaDeclOrMemberId::Member(self.member_id(*id)?),
        })
    }

    fn self_ref_id(&self, id: &SelfRefId) -> Option<SelfRefId> {
        Some(SelfRefId {
            self_decl_id: self.decl_id(id.self_decl_id)?,
            receiver: self.decl_or_member_id(&id.receiver)?,
        })
    }

    /// A variable reference in the coordinates after the edits. A global
    /// name's position is not part of its identity, so it stays.
    fn var_ref_id(&self, id: &VarRefId) -> Option<VarRefId> {
        Some(match id {
            VarRefId::VarRef(decl_id) => VarRefId::VarRef(self.decl_id(*decl_id)?),
            VarRefId::SelfRef(self_ref) => VarRefId::SelfRef(self.self_ref_id(self_ref)?),
            VarRefId::IndexRef(root, path) => VarRefId::IndexRef(
                match root {
                    VarRefRootId::Decl(id) => VarRefRootId::Decl(self.decl_id(*id)?),
                    VarRefRootId::Member(id) => VarRefRootId::Member(self.member_id(*id)?),
                    VarRefRootId::SelfRef(self_ref) => {
                        VarRefRootId::SelfRef(self.self_ref_id(self_ref)?)
                    }
                },
                path.clone(),
            ),
            VarRefId::GlobalName(..) => id.clone(),
        })
    }

    fn member_owner(&self, owner: &LuaMemberOwner) -> Option<LuaMemberOwner> {
        Some(match owner {
            LuaMemberOwner::Element(site) => LuaMemberOwner::Element(self.site(site)?),
            other => other.clone(),
        })
    }

    /// A member key naming the edited files' table literals, in the
    /// coordinates after the edits.
    fn member_key(&self, key: &LuaMemberKey) -> Option<LuaMemberKey> {
        let LuaMemberKey::ExprType(typ) = key else {
            return Some(key.clone());
        };
        let mut typ = typ.clone();
        for (file_id, map) in &self.maps {
            let result = crate::db_index::remap_identities_in_type(
                &typ,
                &crate::FileRemap::unvalidated(*file_id, map.clone()),
            );
            if result.lost {
                return None;
            }
            if let Some(moved) = result.typ {
                typ = moved;
            }
        }
        Some(LuaMemberKey::ExprType(typ))
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
            K::DeclReferences(id) => K::DeclReferences(self.decl_id(*id)?),
            K::Member(id) => K::Member(self.member_id(*id)?),
            K::MemberObject(id) => K::MemberObject(self.member_id(*id)?),
            K::MemberHome(id) => K::MemberHome(self.member_id(*id)?),
            K::MemberMarks(id) => K::MemberMarks(self.member_id(*id)?),
            K::MemberSlot(owner, key) => {
                K::MemberSlot(self.member_owner(owner)?, self.member_key(key)?)
            }
            K::MemberKey(key) => K::MemberKey(self.member_key(key)?),
            K::MemberOwnerMap(owner) => K::MemberOwnerMap(self.member_owner(owner)?),
            K::MemberOwnerExprKeys(owner) => K::MemberOwnerExprKeys(self.member_owner(owner)?),
            K::MemberOwnerFields(owner) => K::MemberOwnerFields(self.member_owner(owner)?),
            K::MemberOwnerPresence(owner) => K::MemberOwnerPresence(self.member_owner(owner)?),
            K::DefinitionSite(site) => K::DefinitionSite(self.site(site)?),
            K::Metatable(site) => K::Metatable(self.site(site)?),
            K::CallTarget(site) => K::CallTarget(self.site(site)?),
            K::CallSiteParam(site) => K::CallSiteParam(self.site(site)?),
            K::TableClass(site) => K::TableClass(self.site(site)?),
            K::DynamicFieldSite(site) => K::DynamicFieldSite(self.site(site)?),
            K::NetCall(site) => K::NetCall(self.site(site)?),
            K::NetHelper(site) => K::NetHelper(self.site(site)?),
            K::NetFlows(site) => K::NetFlows(self.site(site)?),
            K::NetDefinitionUses(site) => K::NetDefinitionUses(self.site(site)?),
            K::Syntax(site) => K::Syntax(self.site(site)?),
            K::FunctionScope(site) => K::FunctionScope(InFiled::new(
                site.file_id,
                self.position(site.file_id, site.value)?,
            )),
            K::Signature(id) => K::Signature(self.signature_id(*id)?),
            K::SignatureDecl(id) => K::SignatureDecl(self.signature_id(*id)?),
            K::SignatureReturnsNever(id) => K::SignatureReturnsNever(self.signature_id(*id)?),
            K::SpecialCallSignature(id) => K::SpecialCallSignature(self.signature_id(*id)?),
            K::ValidGuard(id) => K::ValidGuard(self.signature_id(*id)?),
            K::InferredGuard(id) => K::InferredGuard(self.signature_id(*id)?),
            K::Contrib(id, idx) => K::Contrib(self.signature_id(*id)?, *idx),
            K::ReceiverAuthority(id) => K::ReceiverAuthority(self.signature_id(*id)?),
            K::CallbackParams(id) => K::CallbackParams(self.signature_id(*id)?),
            K::CallRole(super::CallRoleKey::Signature(id)) => {
                K::CallRole(super::CallRoleKey::Signature(self.signature_id(*id)?))
            }
            K::SpecialCall(file_id, position) => {
                K::SpecialCall(*file_id, self.position(*file_id, *position)?)
            }
            K::SpecialCallFor(file_id, var_ref_id) => {
                K::SpecialCallFor(*file_id, self.var_ref_id(var_ref_id)?)
            }
            K::GmodRealmAt(file_id, offset) => {
                K::GmodRealmAt(*file_id, self.position(*file_id, *offset)?)
            }
            K::GmodMemberRealmAt(file_id, offset) => {
                K::GmodMemberRealmAt(*file_id, self.position(*file_id, *offset)?)
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

/// How many threads are fingerprinting with edits mapped back: a cheap guard
/// in front of the thread-local lookup every positional hash makes.
static MAPPING_BACK: AtomicUsize = AtomicUsize::new(0);

fn any_thread_mapping_back() -> bool {
    MAPPING_BACK.load(Ordering::Relaxed) != 0
}

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
    let _restore = super::OnDrop(|| FACT_FILE.with(|current| current.set(outer)));
    f()
}

/// Hashes a position that does not carry its file: one of the file whose
/// fact is being fingerprinted ([`in_file`]).
pub fn hash_local<P: FilePosition + ?Sized, H: Hasher>(position: &P, state: &mut H) {
    let file_id = FACT_FILE.with(Cell::get).unwrap_or(FileId::VIRTUAL);
    position.hash_in_file(file_id, state);
}

/// A position of the file whose fact is being fingerprinted, hashed as
/// [`hash_local`] hashes it.
pub struct Local<'a, P: ?Sized>(pub &'a P);

impl<P: FilePosition + ?Sized> Hash for Local<'_, P> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        hash_local(self.0, state);
    }
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
    any_thread_mapping_back() && BACK_MAPS.with(|maps| maps.borrow().is_some())
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
    MAPPING_BACK.fetch_add(1, Ordering::Relaxed);
    let _restore = super::OnDrop(|| {
        MAPPING_BACK.fetch_sub(1, Ordering::Relaxed);
        BACK_MAPS.with(|maps| *maps.borrow_mut() = None);
    });
    fingerprint()
}

/// Runs `f` with fingerprints taken as they stand, even inside
/// [`fingerprint_before_edits`]: for a value stored while one is taken.
pub fn unmapped<T>(f: impl FnOnce() -> T) -> T {
    let mut maps = BACK_MAPS.with(|maps| maps.borrow_mut().take());
    let _restore = super::OnDrop(move || BACK_MAPS.with(|slot| *slot.borrow_mut() = maps.take()));
    f()
}

/// Hashes a fact value; inside [`fingerprint_before_edits`] its positional
/// identities hash as their pre-edit form. Map lookups made while computing
/// the fingerprint happen outside this and hash normally.
pub(super) fn hash_value<T: Hash + ?Sized, H: Hasher>(value: &T, state: &mut H) {
    let outer = FINGERPRINTING.with(|flag| flag.replace(true));
    let mapping = mapping_back();
    if mapping {
        HASHING.with(|hashing| hashing.set(true));
    }
    let _restore = super::OnDrop(|| {
        if mapping {
            HASHING.with(|hashing| hashing.set(false));
        }
        FINGERPRINTING.with(|flag| flag.set(outer));
    });
    value.hash(state);
}

/// What a positional identity in `file_id` hashes as right now: `None` to
/// hash it as it is.
fn mapped<T>(file_id: FileId, map: impl FnOnce(&EditMaps) -> Option<T>) -> Option<Option<T>> {
    if !any_thread_mapping_back() || !HASHING.with(Cell::get) {
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
