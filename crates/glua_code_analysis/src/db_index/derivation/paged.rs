/// A vector in fixed-size pages: it grows a page at a time, never copying what
/// it holds and never reserving more than one page beyond its length.
#[derive(Debug, Clone)]
pub(super) struct Paged<T> {
    pages: Vec<Vec<T>>,
    len: usize,
}

pub(super) const PAGE: usize = 1 << 16;

impl<T> Default for Paged<T> {
    fn default() -> Self {
        Self {
            pages: Vec::new(),
            len: 0,
        }
    }
}

impl<T> Paged<T> {
    pub(super) fn len(&self) -> usize {
        self.len
    }

    pub(super) fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub(super) fn capacity(&self) -> usize {
        self.pages.len() * PAGE
    }

    /// Appends `value`, returning its index.
    pub(super) fn push(&mut self, value: T) -> usize {
        if self.len == self.capacity() {
            self.pages.push(Vec::with_capacity(PAGE));
        }
        self.pages.last_mut().expect("a page with room").push(value);
        self.len += 1;
        self.len - 1
    }

    pub(super) fn get(&self, index: usize) -> Option<&T> {
        (index < self.len).then(|| &self[index])
    }

    /// The pages, each holding [`PAGE`] consecutive indices.
    pub(super) fn pages(&self) -> &[Vec<T>] {
        &self.pages
    }

    /// The pages, each holding [`PAGE`] consecutive indices.
    pub(super) fn pages_mut(&mut self) -> &mut [Vec<T>] {
        &mut self.pages
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = &T> {
        self.pages.iter().flatten()
    }
}

impl<T> std::ops::Index<usize> for Paged<T> {
    type Output = T;

    fn index(&self, index: usize) -> &T {
        &self.pages[index / PAGE][index % PAGE]
    }
}

impl<T> std::ops::IndexMut<usize> for Paged<T> {
    fn index_mut(&mut self, index: usize) -> &mut T {
        &mut self.pages[index / PAGE][index % PAGE]
    }
}
