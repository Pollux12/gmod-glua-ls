use lsp_types::request::Request;
use serde::{Deserialize, Serialize};

use crate::handlers::emmy_gutter::GutterKind;

#[derive(Debug)]
pub enum EmmyGutterDetailRequest {}

impl Request for EmmyGutterDetailRequest {
    type Params = EmmyGutterDetailParams;
    type Result = Option<GutterDetailResponse>;
    const METHOD: &'static str = "gluals/gutter/detail";
}

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
pub struct EmmyGutterDetailParams {
    pub data: String,
}

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
pub struct GutterLocation {
    pub uri: String,
    pub line: i32,
    pub kind: GutterKind,
}

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
pub struct GutterDetailResponse {
    pub locations: Vec<GutterLocation>,
}
