use lsp_types::request::Request;
use serde::{Deserialize, Serialize};

#[derive(Debug)]
pub enum GmodScriptedClassesRequest {}

impl Request for GmodScriptedClassesRequest {
    type Params = GmodScriptedClassesParams;
    type Result = Option<Vec<GmodScriptedClassEntry>>;
    const METHOD: &'static str = "gluals/gmodScriptedClasses";
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GmodScriptedClassesParams {}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GmodScriptedClassEntry {
    pub uri: String,
    pub class_type: String,
    pub class_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub range: Option<lsp_types::Range>,
}
