use lsp_types::request::Request;
use serde::{Deserialize, Serialize};

use glua_code_analysis::ResolvedGmodScriptedClassDefinition;

#[derive(Debug)]
pub enum GmodScriptedClassesRequest {}

impl Request for GmodScriptedClassesRequest {
    type Params = GmodScriptedClassesParams;
    type Result = Option<Vec<LegacyGmodScriptedClassEntry>>;
    const METHOD: &'static str = "gluals/gmodScriptedClasses";
}

#[derive(Debug)]
pub enum GmodScriptedClassesV2Request {}

impl Request for GmodScriptedClassesV2Request {
    type Params = GmodScriptedClassesParams;
    type Result = Option<GmodScriptedClassesResult>;
    const METHOD: &'static str = "gluals/gmodScriptedClassesV2";
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GmodScriptedClassesParams {}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GmodScriptedClassesResult {
    pub definitions: Vec<ResolvedGmodScriptedClassDefinition>,
    pub entries: Vec<GmodScriptedClassEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LegacyGmodScriptedClassEntry {
    pub uri: String,
    pub class_type: String,
    pub class_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub range: Option<lsp_types::Range>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GmodScriptedClassEntry {
    pub uri: String,
    pub class_type: String,
    pub class_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub definition_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub range: Option<lsp_types::Range>,
}
