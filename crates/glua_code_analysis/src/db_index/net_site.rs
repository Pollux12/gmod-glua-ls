//! A file's net call facts and function-body summaries, each filed under the
//! range of the call or body it describes.

use std::sync::Arc;

use rowan::TextRange;
use rustc_hash::FxHashMap;

use super::derivation::{self, DerivedFact, FactKey, Fingerprint};
use crate::{FileId, InFiled};

/// A stored fact with its fingerprint when stored.
pub(super) type NetSite = (Arc<dyn DerivedFact>, Fingerprint);

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
/// file.
#[derive(Debug, Clone, Default)]
pub(super) struct FileNetSites {
    calls: FxHashMap<TextRange, NetSite>,
    helpers: FxHashMap<TextRange, NetSite>,
    named: FxHashMap<FileId, usize>,
}

/// Two files' net sites are the same when they file the same ranges with the
/// same fingerprints.
impl PartialEq for FileNetSites {
    fn eq(&self, other: &Self) -> bool {
        let same = |left: &FxHashMap<TextRange, NetSite>, right: &FxHashMap<TextRange, NetSite>| {
            left.len() == right.len()
                && left.iter().all(|(range, (_, fingerprint))| {
                    right
                        .get(range)
                        .is_some_and(|(_, other)| other == fingerprint)
                })
        };
        same(&self.calls, &other.calls)
            && same(&self.helpers, &other.helpers)
            && self.named == other.named
    }
}

impl FileNetSites {
    fn table(&self, kind: NetSiteKind) -> &FxHashMap<TextRange, NetSite> {
        match kind {
            NetSiteKind::Call => &self.calls,
            NetSiteKind::Helper => &self.helpers,
        }
    }

    fn table_mut(&mut self, kind: NetSiteKind) -> &mut FxHashMap<TextRange, NetSite> {
        match kind {
            NetSiteKind::Call => &mut self.calls,
            NetSiteKind::Helper => &mut self.helpers,
        }
    }

    pub(super) fn get(&self, kind: NetSiteKind, range: TextRange) -> Option<&NetSite> {
        self.table(kind).get(&range)
    }

    pub(super) fn is_empty(&self) -> bool {
        self.calls.is_empty() && self.helpers.is_empty()
    }

    /// Every site, calls first, each kind in range order.
    pub(super) fn sorted(&self) -> Vec<(NetSiteKind, TextRange, &NetSite)> {
        let mut sites = [NetSiteKind::Call, NetSiteKind::Helper]
            .into_iter()
            .flat_map(|kind| {
                self.table(kind)
                    .iter()
                    .map(move |(range, site)| (kind, *range, site))
            })
            .collect::<Vec<_>>();
        sites.sort_unstable_by_key(|(kind, range, _)| (*kind, range.start(), range.end()));
        sites
    }

    /// The files whose positions some site names.
    pub(super) fn named_files(&self) -> impl Iterator<Item = FileId> + '_ {
        self.named.keys().copied()
    }

    /// Files `site` under `range`, adding to `linked` each file that came to
    /// be named and to `unlinked` each that no site names any more.
    pub(super) fn insert(
        &mut self,
        own: FileId,
        kind: NetSiteKind,
        range: TextRange,
        site: NetSite,
        linked: &mut Vec<FileId>,
        unlinked: &mut Vec<FileId>,
    ) {
        for file_id in site.0.named_files(own) {
            let count = self.named.entry(file_id).or_default();
            *count += 1;
            if *count == 1 {
                linked.push(file_id);
            }
        }
        if let Some((previous, _)) = self.table_mut(kind).insert(range, site) {
            self.unname(own, &*previous, unlinked);
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
        let Some((previous, _)) = self.table_mut(kind).remove(&range) else {
            return false;
        };
        self.unname(own, &*previous, unlinked);
        true
    }

    fn unname(&mut self, own: FileId, fact: &dyn DerivedFact, unlinked: &mut Vec<FileId>) {
        for file_id in fact.named_files(own) {
            let Some(count) = self.named.get_mut(&file_id) else {
                continue;
            };
            *count -= 1;
            if *count == 0 {
                self.named.remove(&file_id);
                unlinked.push(file_id);
            }
        }
    }

    /// The sites as one fingerprint, for comparing two analyses.
    pub(super) fn fingerprint(&self, file_id: FileId) -> Fingerprint {
        derivation::in_file(Some(file_id), || {
            self.sorted()
                .into_iter()
                .map(|(kind, range, (_, fingerprint))| {
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
