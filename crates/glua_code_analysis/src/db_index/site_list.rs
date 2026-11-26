use std::ops::{Deref, DerefMut};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SiteList<T> {
    One(T),
    Many(Box<[T]>),
}

impl<T> Default for SiteList<T> {
    fn default() -> Self {
        Self::Many(Box::default())
    }
}

impl<T> From<Vec<T>> for SiteList<T> {
    fn from(mut sites: Vec<T>) -> Self {
        match sites.len() {
            1 => Self::One(sites.remove(0)),
            _ => Self::Many(sites.into_boxed_slice()),
        }
    }
}

impl<T> Deref for SiteList<T> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        match self {
            Self::One(site) => std::slice::from_ref(site),
            Self::Many(sites) => sites,
        }
    }
}

impl<T> DerefMut for SiteList<T> {
    fn deref_mut(&mut self) -> &mut [T] {
        match self {
            Self::One(site) => std::slice::from_mut(site),
            Self::Many(sites) => sites,
        }
    }
}

impl<T> SiteList<T> {
    pub(crate) fn push(&mut self, site: T) {
        self.insert(self.len(), site);
    }

    pub(crate) fn insert(&mut self, index: usize, site: T) {
        *self = match std::mem::take(self) {
            Self::One(old) if index == 0 => Self::Many(Box::new([site, old])),
            Self::One(old) => Self::Many(Box::new([old, site])),
            Self::Many(sites) if sites.is_empty() => Self::One(site),
            Self::Many(sites) => {
                let mut updated = Vec::with_capacity(sites.len() + 1);
                updated.extend(sites);
                updated.insert(index, site);
                Self::Many(updated.into_boxed_slice())
            }
        };
    }
}

impl<'a, T> IntoIterator for &'a SiteList<T> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}
