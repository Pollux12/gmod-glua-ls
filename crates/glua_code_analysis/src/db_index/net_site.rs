//! A file's net call facts and function-body summaries, each filed under the
//! range of the call or body it describes.

use std::sync::Arc;

use glua_parser::{LuaKind, LuaSyntaxId, LuaSyntaxKind};
use rowan::TextRange;
use rustc_hash::FxHashMap;

use super::derivation::{self, DerivedFact, FactKey, Fingerprint};
use super::stored_fact_fingerprint;
use crate::{FileId, InFiled};

/// A stored fact with its fingerprint when stored.
pub(super) type NetSite = (Arc<dyn DerivedFact>, Fingerprint);

/// Compact storage for a call's net fact. Most calls have no net role and are
/// not net-helper candidates, carrying only a target function body — stored
/// inline in 16 bytes with no heap allocation.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum StoredNetCall {
    TargetOnly(InFiled<TextRange>),
    Full(Box<NetSite>),
}

const _: () = assert!(std::mem::size_of::<StoredNetCall>() == 16);

impl StoredNetCall {
    pub(super) fn fingerprint(&self, own: FileId) -> Fingerprint {
        match self {
            Self::TargetOnly(target) => target_only_fingerprint(own, target),
            Self::Full(site) => stored_fact_fingerprint(site),
        }
    }

    pub(super) fn remap(&self, remap: &crate::FileRemap) -> crate::Remap<Self> {
        match self {
            Self::TargetOnly(target) => {
                let block = LuaSyntaxId::new(LuaKind::Syntax(LuaSyntaxKind::Block), target.value);
                match remap.syntax_id(target.file_id, block) {
                    crate::Remap::Moved(block) if block.get_range() != target.value => {
                        crate::Remap::Moved(Self::TargetOnly(InFiled::new(
                            target.file_id,
                            block.get_range(),
                        )))
                    }
                    crate::Remap::Lost => crate::Remap::Lost,
                    crate::Remap::Moved(_) | crate::Remap::Unrelated => crate::Remap::Unrelated,
                }
            }
            Self::Full(site) => match site.0.remap(remap) {
                crate::Remap::Moved(fact) => {
                    let fingerprint = fact.fingerprint().max(1);
                    crate::Remap::Moved(Self::Full(Box::new((fact, fingerprint))))
                }
                crate::Remap::Lost => crate::Remap::Lost,
                crate::Remap::Unrelated => crate::Remap::Unrelated,
            },
        }
    }
}

fn target_only_fingerprint(own: FileId, target: &InFiled<TextRange>) -> Fingerprint {
    derivation::in_file(Some(own), || derivation::fingerprint_of(&(0u8, target))).max(1)
}

/// What a net site describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum NetSiteKind {
    /// A call: its net role and the function body it expands into.
    Call,
    /// A function body: its net summary, by the body's block.
    Helper,
}

impl NetSiteKind {
    pub(super) fn key(self, file_id: FileId, range: TextRange) -> FactKey {
        let site = InFiled::new(file_id, range);
        match self {
            Self::Call => FactKey::NetCall(site),
            Self::Helper => FactKey::NetHelper(site),
        }
    }
}

/// One file's net sites, and how many of them name a position in each other
/// file. Two are the same when they file the same stored facts.
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct FileNetSites {
    calls: FxHashMap<TextRange, StoredNetCall>,
    helpers: FxHashMap<TextRange, NetSite>,
    named: FxHashMap<FileId, usize>,
}

impl FileNetSites {
    pub(super) fn get_call(&self, range: TextRange) -> Option<&StoredNetCall> {
        self.calls.get(&range)
    }

    pub(super) fn get_helper(&self, range: TextRange) -> Option<&NetSite> {
        self.helpers.get(&range)
    }

    pub(super) fn contains(&self, kind: NetSiteKind, range: TextRange) -> bool {
        match kind {
            NetSiteKind::Call => self.calls.contains_key(&range),
            NetSiteKind::Helper => self.helpers.contains_key(&range),
        }
    }

    pub(super) fn site_fingerprint(
        &self,
        own: FileId,
        kind: NetSiteKind,
        range: TextRange,
    ) -> Option<Fingerprint> {
        match kind {
            NetSiteKind::Call => self.calls.get(&range).map(|call| call.fingerprint(own)),
            NetSiteKind::Helper => self.helpers.get(&range).map(stored_fact_fingerprint),
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.calls.is_empty() && self.helpers.is_empty()
    }

    /// Every call site, in range order.
    pub(super) fn sorted_calls(&self) -> Vec<(TextRange, &StoredNetCall)> {
        let mut sites = self
            .calls
            .iter()
            .map(|(range, call)| (*range, call))
            .collect::<Vec<_>>();
        sites.sort_unstable_by_key(|(range, _)| (range.start(), range.end()));
        sites
    }

    /// Every helper site, in range order.
    pub(super) fn sorted_helpers(&self) -> Vec<(TextRange, &NetSite)> {
        let mut sites = self
            .helpers
            .iter()
            .map(|(range, site)| (*range, site))
            .collect::<Vec<_>>();
        sites.sort_unstable_by_key(|(range, _)| (range.start(), range.end()));
        sites
    }

    /// Every site key, calls first, each kind in range order.
    pub(super) fn sorted_keys(&self) -> Vec<(NetSiteKind, TextRange)> {
        let mut keys = self
            .calls
            .keys()
            .map(|range| (NetSiteKind::Call, *range))
            .chain(
                self.helpers
                    .keys()
                    .map(|range| (NetSiteKind::Helper, *range)),
            )
            .collect::<Vec<_>>();
        keys.sort_unstable_by_key(|(kind, range)| (*kind, range.start(), range.end()));
        keys
    }

    /// The files whose positions some site names.
    pub(super) fn named_files(&self) -> impl Iterator<Item = FileId> + '_ {
        self.named.keys().copied()
    }

    /// Files a call `site` under `range`, updating `linked` and `unlinked`.
    pub(super) fn insert_call(
        &mut self,
        own: FileId,
        range: TextRange,
        call: StoredNetCall,
        linked: &mut Vec<FileId>,
        unlinked: &mut Vec<FileId>,
    ) {
        self.name_call(own, &call, linked);
        if let Some(previous) = self.calls.insert(range, call) {
            self.unname_call(own, &previous, unlinked);
        }
    }

    /// Files a helper `site` under `range`, adding to `linked` each file that
    /// came to be named and to `unlinked` each that no site names any more.
    pub(super) fn insert_helper(
        &mut self,
        own: FileId,
        range: TextRange,
        site: NetSite,
        linked: &mut Vec<FileId>,
        unlinked: &mut Vec<FileId>,
    ) {
        self.name_fact(own, &*site.0, linked);
        if let Some((previous, _)) = self.helpers.insert(range, site) {
            self.unname_fact(own, &*previous, unlinked);
        }
    }

    /// Drops the site under `range`, adding to `unlinked` each file that no
    /// site names any more. Returns whether there was one.
    pub(super) fn remove(
        &mut self,
        own: FileId,
        kind: NetSiteKind,
        range: TextRange,
        unlinked: &mut Vec<FileId>,
    ) -> bool {
        match kind {
            NetSiteKind::Call => {
                let Some(previous) = self.calls.remove(&range) else {
                    return false;
                };
                self.unname_call(own, &previous, unlinked);
                true
            }
            NetSiteKind::Helper => {
                let Some((previous, _)) = self.helpers.remove(&range) else {
                    return false;
                };
                self.unname_fact(own, &*previous, unlinked);
                true
            }
        }
    }

    fn name_file(&mut self, file_id: FileId, linked: &mut Vec<FileId>) {
        let count = self.named.entry(file_id).or_default();
        *count += 1;
        if *count == 1 {
            linked.push(file_id);
        }
    }

    fn unname_file(&mut self, file_id: FileId, unlinked: &mut Vec<FileId>) {
        let Some(count) = self.named.get_mut(&file_id) else {
            return;
        };
        *count -= 1;
        if *count == 0 {
            self.named.remove(&file_id);
            unlinked.push(file_id);
        }
    }

    fn name_call(&mut self, own: FileId, call: &StoredNetCall, linked: &mut Vec<FileId>) {
        match call {
            StoredNetCall::TargetOnly(target) => {
                if target.file_id != own {
                    self.name_file(target.file_id, linked);
                }
            }
            StoredNetCall::Full(site) => self.name_fact(own, &*site.0, linked),
        }
    }

    fn unname_call(&mut self, own: FileId, call: &StoredNetCall, unlinked: &mut Vec<FileId>) {
        match call {
            StoredNetCall::TargetOnly(target) => {
                if target.file_id != own {
                    self.unname_file(target.file_id, unlinked);
                }
            }
            StoredNetCall::Full(site) => self.unname_fact(own, &*site.0, unlinked),
        }
    }

    fn name_fact(&mut self, own: FileId, fact: &dyn DerivedFact, linked: &mut Vec<FileId>) {
        for file_id in fact.named_files(own) {
            self.name_file(file_id, linked);
        }
    }

    fn unname_fact(&mut self, own: FileId, fact: &dyn DerivedFact, unlinked: &mut Vec<FileId>) {
        for file_id in fact.named_files(own) {
            self.unname_file(file_id, unlinked);
        }
    }

    /// The sites as one fingerprint, for comparing two analyses.
    pub(super) fn fingerprint(&self, file_id: FileId) -> Fingerprint {
        derivation::in_file(Some(file_id), || {
            let calls = self
                .sorted_calls()
                .into_iter()
                .map(|(range, call)| (NetSiteKind::Call, range, call.fingerprint(file_id)));
            let helpers = self
                .sorted_helpers()
                .into_iter()
                .map(|(range, (_, fp))| (NetSiteKind::Helper, range, *fp));
            calls
                .chain(helpers)
                .map(|(kind, range, fingerprint)| {
                    derivation::fingerprint_of(&(
                        kind as u8,
                        derivation::Local(&range),
                        fingerprint,
                    ))
                })
                .fold(0u64, u64::wrapping_add)
                .max(1)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fact;

    impl DerivedFact for Fact {
        fn fingerprint(&self) -> Fingerprint {
            1
        }
    }

    fn sites_with(fact: Arc<dyn DerivedFact>) -> FileNetSites {
        let mut sites = FileNetSites::default();
        sites.insert_call(
            FileId::new(1),
            TextRange::new(0.into(), 4.into()),
            StoredNetCall::Full(Box::new((fact, 1))),
            &mut Vec::new(),
            &mut Vec::new(),
        );
        sites
    }

    /// Sites that file another fact under the same fingerprint are not the
    /// same: an install that took them for unchanged would keep the old fact.
    #[test]
    fn sites_filing_another_fact_with_the_same_fingerprint_differ() {
        let fact: Arc<dyn DerivedFact> = Arc::new(Fact);

        assert_eq!(sites_with(fact.clone()), sites_with(fact));
        assert_ne!(sites_with(Arc::new(Fact)), sites_with(Arc::new(Fact)));
    }
}
