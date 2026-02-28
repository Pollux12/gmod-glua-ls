#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u32)]
pub enum PropertyDeclFeature {
    ReadOnly = 1 << 0,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeclFeatureFlag(u32);

impl DeclFeatureFlag {
    pub fn new() -> Self {
        Self(0)
    }

    pub fn add_feature(&mut self, feature: PropertyDeclFeature) {
        self.0 |= feature as u32;
    }

    pub fn has_feature(&self, feature: PropertyDeclFeature) -> bool {
        (self.0 & (feature as u32)) != 0
    }
}
