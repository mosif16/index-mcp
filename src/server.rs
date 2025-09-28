use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{self, AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};

use crate::config::{Config, LogLevel};
use crate::ingest::IngestSummary;
use crate::rpc::{self, RpcRequest, RpcResponse};
use crate::tool::{CallToolParams, Tool, ToolContext, ToolInfo};
use crate::tools::register_all_tools;

#[derive(Clone)]
pub struct ServerState {
    pub working_dir: std::path::PathBuf,
    pub default_database_name: String,
    pub log_level: LogLevel,
    pub last_ingest: Mutex<Option<IngestSummary>>,
}

pub struct Server {
    config: Config,
    state: Arc<ServerState>,
    tools: HashMap<String, Arc<dyn Tool>>,
    tool_infos: Vec<ToolInfo>,
    prompts: Vec<PromptDefinition>,
}

impl Server {
    pub fn new(config: Config) -> Self {
        let state = Arc::new(ServerState {
            working_dir: config.working_dir.clone(),
            default_database_name: config.default_database_name.clone(),
            log_level: config.log_level,
            last_ingest: Mutex::new(None),
        });

        let mut tool_handlers = HashMap::new();
        let mut tool_infos = Vec::new();
        for tool in register_all_tools(state.clone()) {
            let info = tool.info();
            tool_infos.push(info.clone());
            tool_handlers.insert(info.name.clone(), tool);
        }

        Server {
            config,
            state,
            tools: tool_handlers,
            tool_infos,
            prompts: default_prompts(),
        }
    }

    pub async fn run(&self) -> Result<()> {
        self.log(
            LogLevel::Info,
            format!(
                "starting index-mcp server (working dir: {})",
                self.state.working_dir.display()
            ),
        );

        let stdin = io::stdin();
        let stdout = io::stdout();

        let mut reader = BufReader::new(stdin).lines();
        let mut writer = BufWriter::new(stdout);

        while let Some(line) = reader.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }

            match serde_json::from_str::<RpcRequest>(&line) {
                Ok(request) => {
                    if let Some(response) = self.handle_request(request).await {
                        let payload = serde_json::to_vec(&response)
                            .context("Failed to serialise response")?;
                        writer.write_all(&payload).await?;
                        writer.write_all(b"\n").await?;
                        writer.flush().await?;
                    }
                }
                Err(error) => {
                    let response = RpcResponse::error(
                        None,
                        rpc::PARSE_ERROR,
                        format!("Failed to parse request: {error}"),
                        None,
                    );
                    let payload = serde_json::to_vec(&response)?;
                    writer.write_all(&payload).await?;
                    writer.write_all(b"\n").await?;
                    writer.flush().await?;
                }
            }
        }

        Ok(())
    }

    fn log(&self, level: LogLevel, message: impl AsRef<str>) {
        let requested = self.config.log_level.precedence();
        let current = level.precedence();
        if current <= requested {
            eprintln!("[index-mcp] {}", message.as_ref());
        }
    }

    async fn handle_request(&self, request: RpcRequest) -> Option<RpcResponse> {
        match request.method.as_str() {
            "initialize" => Some(self.handle_initialize(request)),
            "list_tools" => Some(self.handle_list_tools(request)),
            "call_tool" => Some(self.handle_call_tool(request).await),
            "list_prompts" => Some(self.handle_list_prompts(request)),
            "get_prompt" => Some(self.handle_get_prompt(request)),
            "ping" => Some(RpcResponse::result(request.id, json!({ "ok": true }))),
            "shutdown" => Some(RpcResponse::result(request.id, json!({}))),
            _ => {
                if request.id.is_none() {
                    self.log(
                        LogLevel::Debug,
                        format!("Ignoring notification {}", request.method),
                    );
                    None
                } else {
                    Some(RpcResponse::error(
                        request.id,
                        rpc::METHOD_NOT_FOUND,
                        format!("Unknown method {}", request.method),
                        None,
                    ))
                }
            }
        }
    }

    fn handle_initialize(&self, request: RpcRequest) -> RpcResponse {
        let result = json!({
            "protocolVersion": "2024-05-31",
            "serverInfo": {
                "name": "index-mcp",
                "version": env!("CARGO_PKG_VERSION"),
                "description": "Rust implementation of the index MCP server",
            },
            "capabilities": {
                "tools": { "listChanged": true },
                "prompts": { "listChanged": true },
                "logging": { "level": self.config.log_level.as_str() },
            }
        });
        RpcResponse::result(request.id, result)
    }

    fn handle_list_tools(&self, request: RpcRequest) -> RpcResponse {
        let tools: Vec<Value> = self
            .tool_infos
            .iter()
            .map(|info| serde_json::to_value(info).unwrap_or_else(|_| json!({})))
            .collect();
        RpcResponse::result(request.id, json!({ "tools": tools }))
    }

    async fn handle_call_tool(&self, request: RpcRequest) -> RpcResponse {
        let id = request.id.clone();
        let params_value = request.params.unwrap_or_else(|| json!({}));
        let params: CallToolParams = match serde_json::from_value(params_value.clone()) {
            Ok(value) => value,
            Err(error) => {
                return RpcResponse::error(
                    id,
                    rpc::INVALID_PARAMS,
                    format!("Invalid call_tool parameters: {error}"),
                    Some(params_value),
                );
            }
        };

        let tool = match self.tools.get(&params.name) {
            Some(tool) => Arc::clone(tool),
            None => {
                return RpcResponse::error(
                    id,
                    rpc::INVALID_PARAMS,
                    format!("Unknown tool {}", params.name),
                    None,
                )
            }
        };

        let context = ToolContext::new(self.state.clone(), params.extra.clone());
        match tool.call(params.arguments.clone(), context).await {
            Ok(response) => match serde_json::to_value(response) {
                Ok(value) => RpcResponse::result(id, value),
                Err(error) => RpcResponse::error(
                    id,
                    rpc::INTERNAL_ERROR,
                    format!("Failed to serialise tool result: {error}"),
                    None,
                ),
            },
            Err(error) => RpcResponse::error(
                id,
                rpc::INTERNAL_ERROR,
                format!("Tool {} failed: {error}", params.name),
                None,
            ),
        }
    }

    fn handle_list_prompts(&self, request: RpcRequest) -> RpcResponse {
        let prompts: Vec<Value> = self
            .prompts
            .iter()
            .map(|prompt| {
                json!({
                    "name": prompt.name,
                    "description": prompt.description,
                })
            })
            .collect();
        RpcResponse::result(request.id, json!({ "prompts": prompts }))
    }

    fn handle_get_prompt(&self, request: RpcRequest) -> RpcResponse {
        let params_value = request.params.unwrap_or_else(|| json!({}));
        let params: GetPromptParams = match serde_json::from_value(params_value.clone()) {
            Ok(value) => value,
            Err(error) => {
                return RpcResponse::error(
                    request.id,
                    rpc::INVALID_PARAMS,
                    format!("Invalid get_prompt parameters: {error}"),
                    Some(params_value),
                );
            }
        };

        match self
            .prompts
            .iter()
            .find(|prompt| prompt.name == params.name)
        {
            Some(prompt) => RpcResponse::result(request.id, json!({ "prompt": prompt })),
            None => RpcResponse::error(
                request.id,
                rpc::INVALID_PARAMS,
                format!("Prompt {} not found", params.name),
                None,
            ),
        }
    }

    pub fn state(&self) -> Arc<ServerState> {
        self.state.clone()
    }
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct PromptDefinition {
    name: String,
    description: String,
    messages: Vec<PromptMessage>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct PromptMessage {
    role: String,
    content: Vec<PromptContent>,
}

#[derive(Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum PromptContent {
    Text { text: String },
}

#[derive(Debug, Deserialize)]
struct GetPromptParams {
    name: String,
}

fn default_prompts() -> Vec<PromptDefinition> {
    vec![PromptDefinition {
        name: "indexing_guidance".to_string(),
        description: "Reminders for keeping the SQLite index current.".to_string(),
        messages: vec![PromptMessage {
            role: "system".to_string(),
            content: vec![PromptContent::Text {
                text: "Run ingest_codebase when you first connect to a project and after you modify files. The server stores a SQLite database (default .mcp-index.sqlite) at the workspace root. Use code_lookup for retrieving files or substring matches once an index exists, and index_status to confirm the most recent ingest summary.".to_string(),
            }],
        }],
    }]
}
