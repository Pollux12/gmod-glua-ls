use lsp_types::request::Request;
use serde::{Deserialize, Serialize};

#[derive(Debug)]
pub enum GluaDocSearchRequest {}

impl Request for GluaDocSearchRequest {
    type Params = GluaDocSearchParams;
    type Result = Option<GluaDocSearchResponse>;
    const METHOD: &'static str = "gluals/docSearch";
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GluaDocSearchParams {
    pub query: String,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GluaDocSearchResponse {
    pub items: Vec<GluaDocItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GluaDocItem {
    pub name: String,
    pub full_name: String,
    pub kind: String,
    pub documentation: String,
    pub deprecated: bool,
}
