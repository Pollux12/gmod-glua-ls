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
/// file. Two are the same when they file the same stored facts.
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct FileNetSites {
    calls: FxHashMap<TextRange, NetSite>,
    helpers: FxHashMap<TextRange, NetSite>,
    named: FxHashMap<FileId, usize>,
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
        sites.insert(
            FileId::new(1),
            NetSiteKind::Call,
            TextRange::new(0.into(), 4.into()),
            (fact, 1),
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
