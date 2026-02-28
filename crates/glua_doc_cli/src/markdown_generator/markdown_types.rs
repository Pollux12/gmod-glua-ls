use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct Doc {
    pub name: String,
    pub display: Option<String>,
    pub supers: Option<String>,
    pub namespace: Option<String>,
    pub fields: Option<Vec<MemberDoc>>,
    pub methods: Option<Vec<MemberDoc>>,
    pub property: Property,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MemberDoc {
    pub name: String,
    pub display: String,
    pub property: Property,
}

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct Property {
    pub description: Option<String>,
    pub see: Option<String>,
    pub deprecated: Option<String>,
    pub other: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct MkdocsIndex {
    pub site_name: String,
    pub types: Vec<IndexStruct>,
    pub modules: Vec<IndexStruct>,
    pub globals: Vec<IndexStruct>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct IndexStruct {
    pub name: String,
    pub file: String,
}
