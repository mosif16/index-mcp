use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::server::ServerState;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolInfo {
    pub name: String,
    pub description: String,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolContent {
    Text {
        text: String,
    },
    #[serde(rename_all = "camelCase")]
    Json {
        data: Value,
    },
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResponse {
    pub content: Vec<ToolContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub structured_output: Option<Value>,
}

impl ToolResponse {
    pub fn text(message: impl Into<String>) -> Self {
        ToolResponse {
            content: vec![ToolContent::Text {
                text: message.into(),
            }],
            structured_output: None,
        }
    }

    pub fn with_structured(message: impl Into<String>, structured: Value) -> Self {
        ToolResponse {
            content: vec![ToolContent::Text {
                text: message.into(),
            }],
            structured_output: Some(structured),
        }
    }
}

#[derive(Clone)]
pub struct ToolContext {
    pub state: Arc<ServerState>,
    pub raw_request: Value,
}

impl ToolContext {
    pub fn new(state: Arc<ServerState>, raw_request: Value) -> Self {
        ToolContext { state, raw_request }
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn info(&self) -> ToolInfo;
    async fn call(&self, args: Value, ctx: ToolContext) -> Result<ToolResponse>;
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallToolParams {
    pub name: String,
    #[serde(default)]
    pub arguments: Value,
    #[serde(default)]
    pub extra: Value,
}

impl Default for CallToolParams {
    fn default() -> Self {
        CallToolParams {
            name: String::new(),
            arguments: json!({}),
            extra: json!({}),
        }
    }
}
