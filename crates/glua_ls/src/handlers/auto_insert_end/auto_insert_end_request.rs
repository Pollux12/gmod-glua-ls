use lsp_types::{Position, request::Request};
use serde::{Deserialize, Serialize};

#[derive(Debug)]
pub enum AutoInsertEndRequest {}

impl Request for AutoInsertEndRequest {
    type Params = AutoInsertEndParams;
    type Result = Option<AutoInsertEndResponse>;
    const METHOD: &'static str = "gluals/autoInsertEnd";
}

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
pub struct AutoInsertEndParams {
    pub uri: String,
    pub position: Position,
    pub version: i32,
}

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AutoInsertEndResponse {
    pub should_insert: bool,
    pub close_keyword: String,
    pub block_kind: Option<String>,
    pub reason: Option<String>,
}
