#[derive(Debug, Copy, Clone)]
pub struct SemanticDeclGuard {
    level: i32,
    limit: i32,
    /// Whether the lookup reached the current class from one of its subclasses.
    inherited: bool,
}

impl SemanticDeclGuard {
    pub fn new(limit: i32) -> Self {
        Self {
            level: 0,
            limit,
            inherited: false,
        }
    }

    pub fn next_level(&self) -> Option<Self> {
        self.descend(self.inherited)
    }

    /// The next level of a lookup that moves from a class to one of its supers.
    pub fn next_super_level(&self) -> Option<Self> {
        self.descend(true)
    }

    pub fn is_inherited(&self) -> bool {
        self.inherited
    }

    fn descend(&self, inherited: bool) -> Option<Self> {
        if self.level > self.limit {
            return None;
        }

        Some(Self {
            level: self.level + 1,
            limit: self.limit,
            inherited,
        })
    }

    pub fn reached_limit(&self) -> bool {
        self.level >= self.limit
    }
}

impl Default for SemanticDeclGuard {
    fn default() -> Self {
        Self::new(50)
    }
}
