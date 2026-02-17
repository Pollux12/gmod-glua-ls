use lsp_types::request::Request;
use serde::{Deserialize, Serialize};

#[derive(Debug)]
pub enum EmmySyntaxTreeRequest {}

impl Request for EmmySyntaxTreeRequest {
    type Params = EmmySyntaxTreeParams;
    type Result = Option<SyntaxTreeResponse>;
    const METHOD: &'static str = "gluals/syntaxTree";
}

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
pub struct EmmySyntaxTreeParams {
    pub uri: String,
}

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
pub struct SyntaxTreeResponse {
    pub content: String,
}
