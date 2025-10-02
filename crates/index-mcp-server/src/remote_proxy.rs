use std::collections::HashMap;
use std::env;
use std::future::Future;
use std::sync::Arc;

use once_cell::sync::Lazy;
use regex::Regex;
use rmcp::model::{
    CallToolRequestParam, CallToolResult, ClientResult, JsonObject, ServerNotification,
    ServerRequest, Tool,
};
use rmcp::service::{
    self, serve_client, NotificationContext, Peer, RequestContext, RunningService, Service,
    ServiceError,
};
use rmcp::transport::SseClientTransport;
use serde::Deserialize;
use tokio::sync::Mutex;
use tokio::time::{sleep, Duration};
use tracing::{debug, info, warn};

use rmcp::ErrorData as McpError;
use rmcp::RoleClient;

static WHITESPACE_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\s+").unwrap());

const REMOTE_CONFIG_ENV: &str = "INDEX_MCP_REMOTE_SERVERS";

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct RemoteServerConfig {
    pub name: String,
    #[serde(default)]
    pub namespace: Option<String>,
    pub url: String,
    #[serde(default)]
    pub headers: Option<HashMap<String, String>>,
    #[serde(default)]
    pub auth: Option<AuthConfig>,
    #[serde(default)]
    pub retry: Option<RetryConfig>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum AuthConfig {
    Bearer {
        #[serde(default)]
        token: Option<String>,
        #[serde(default, rename = "tokenEnv")]
        token_env: Option<String>,
        #[serde(default)]
        header: Option<String>,
    },
    Header {
        header: String,
        #[serde(default)]
        value: Option<String>,
        #[serde(default, rename = "valueEnv")]
        value_env: Option<String>,
    },
}

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct RetryConfig {
    #[serde(default)]
    pub max_attempts: Option<u32>,
    #[serde(default)]
    pub initial_delay_ms: Option<u64>,
    #[serde(default)]
    pub max_delay_ms: Option<u64>,
    #[serde(default)]
    pub backoff_multiplier: Option<f64>,
}

#[derive(Clone)]
pub struct RemoteProxyRegistry {
    proxies: Vec<Arc<RemoteServerProxy>>,
}

pub struct RemoteToolDescriptor {
    pub proxy: Arc<RemoteServerProxy>,
    pub remote_name: String,
    pub tool: Tool,
}

impl RemoteProxyRegistry {
    pub async fn initialize() -> Self {
        let mut proxies = Vec::new();
        match load_remote_server_configs() {
            Ok(configs) => {
                for config in configs {
                    proxies.push(Arc::new(RemoteServerProxy::new(config)));
                }
            }
            Err(error) => warn!(?error, "Failed to parse remote server configuration"),
        }

        Self { proxies }
    }

    pub async fn tool_descriptors(&self) -> Vec<RemoteToolDescriptor> {
        let mut descriptors = Vec::new();
        for proxy in &self.proxies {
            match proxy.prepare_tool_descriptors().await {
                Ok(mut list) => descriptors.append(&mut list),
                Err(error) => warn!(
                    ?error,
                    namespace = proxy.namespace(),
                    "Failed to fetch remote tool list"
                ),
            }
        }
        descriptors
    }

}

pub struct RemoteServerProxy {
    config: RemoteServerConfig,
    state: Mutex<Option<RemoteClientState>>,
    retry_policy: RetryPolicy,
}

struct RemoteClientState {
    service: RunningService<RoleClient, RemoteClientHandler>,
    peer: Peer<RoleClient>,
}

impl RemoteServerProxy {
    fn new(config: RemoteServerConfig) -> Self {
        let retry_policy = RetryPolicy::from_config(config.retry.as_ref());
        Self {
            config,
            state: Mutex::new(None),
            retry_policy,
        }
    }

    pub fn namespace(&self) -> &str {
        self.config
            .namespace
            .as_deref()
            .unwrap_or(&self.config.name)
    }

    async fn prepare_tool_descriptors(
        self: &Arc<Self>,
    ) -> Result<Vec<RemoteToolDescriptor>, RemoteProxyError> {
        let tools = self.fetch_remote_tools().await?;
        let mut descriptors = Vec::new();
        for tool in tools {
            let proxy = Arc::clone(self);
            let remote_name = tool.name.to_string();
            let namespaced_name = format!("{}.{remote_name}", self.namespace());
            let mut tool_descriptor = tool.clone();
            tool_descriptor.name = namespaced_name.clone().into();

            descriptors.push(RemoteToolDescriptor {
                proxy,
                remote_name,
                tool: tool_descriptor,
            });
        }
        Ok(descriptors)
    }

    async fn fetch_remote_tools(&self) -> Result<Vec<Tool>, RemoteProxyError> {
        self.retry_policy
            .execute(|_attempt| {
                let this = self;
                async move {
                    let peer = this.ensure_peer().await?;
                    match peer.list_all_tools().await {
                        Ok(tools) => Ok(tools.into_iter().map(sanitize_tool).collect()),
                        Err(error) => {
                            this.teardown().await;
                            Err(RemoteProxyError::Service(error))
                        }
                    }
                }
            })
            .await
    }

    pub async fn call_tool(
        &self,
        tool_name: &str,
        arguments: JsonObject,
    ) -> Result<CallToolResult, McpError> {
        let arguments = Arc::new(arguments);

        self.retry_policy
            .execute(|_attempt| {
                let this = self;
                let name = tool_name.to_string();
                let arguments = Arc::clone(&arguments);
                async move {
                    let peer = this.ensure_peer().await?;
                    match peer
                        .call_tool(CallToolRequestParam {
                            name: name.clone().into(),
                            arguments: Some((*arguments).clone()),
                        })
                        .await
                    {
                        Ok(result) => Ok(result),
                        Err(error) => {
                            this.teardown().await;
                            Err(RemoteProxyError::Service(error))
                        }
                    }
                }
            })
            .await
            .map_err(|error| error.into_mcp())
    }

    async fn ensure_peer(&self) -> Result<Peer<RoleClient>, RemoteProxyError> {
        {
            let guard = self.state.lock().await;
            if let Some(state) = guard.as_ref() {
                return Ok(state.peer.clone());
            }
        }

        let new_state = self
            .retry_policy
            .execute(|_attempt| async { self.initialize_client().await })
            .await?;

        let peer = new_state.peer.clone();

        let mut guard = self.state.lock().await;
        *guard = Some(new_state);

        Ok(peer)
    }

    async fn initialize_client(&self) -> Result<RemoteClientState, RemoteProxyError> {
        let reqwest_client = build_reqwest_client(&self.config)?;
        let transport = build_transport(&self.config, reqwest_client).await?;

        let handler = RemoteClientHandler {
            server_name: self.config.name.clone(),
        };

        let running = serve_client(handler, transport)
            .await
            .map_err(RemoteProxyError::ClientInit)?;
        let peer = running.peer().clone();

        Ok(RemoteClientState {
            service: running,
            peer,
        })
    }

    async fn teardown(&self) {
        let mut guard = self.state.lock().await;
        if let Some(state) = guard.take() {
            state.service.cancellation_token().cancel();
        }
    }
}

fn sanitize_tool(mut tool: Tool) -> Tool {
    if let Some(description) = tool.description.as_mut() {
        let original = description.to_string();
        let cleaned = WHITESPACE_RE.replace_all(&original, " ").into_owned();
        if cleaned != original {
            *description = std::borrow::Cow::Owned(cleaned);
        }
    }
    tool
}

fn load_remote_server_configs() -> Result<Vec<RemoteServerConfig>, RemoteProxyError> {
    let raw = match env::var(REMOTE_CONFIG_ENV) {
        Ok(value) if !value.trim().is_empty() => value,
        _ => return Ok(Vec::new()),
    };

    let configs: Vec<RemoteServerConfig> =
        serde_json::from_str(&raw).map_err(|error| RemoteProxyError::Config(error.to_string()))?;
    Ok(configs)
}

fn build_reqwest_client(config: &RemoteServerConfig) -> Result<reqwest::Client, RemoteProxyError> {
    let mut builder = reqwest::Client::builder();
    let mut header_map = reqwest::header::HeaderMap::new();

    if let Some(headers) = &config.headers {
        for (key, value) in headers {
            if let (Ok(name), Ok(value)) = (
                reqwest::header::HeaderName::from_bytes(key.as_bytes()),
                reqwest::header::HeaderValue::from_str(value),
            ) {
                header_map.insert(name, value);
            }
        }
    }

    if let Some(auth_headers) = resolve_auth_headers(config)? {
        for (name, value) in auth_headers {
            header_map.insert(name, value);
        }
    }

    if !header_map.is_empty() {
        builder = builder.default_headers(header_map);
    }

    builder
        .build()
        .map_err(|error| RemoteProxyError::Config(error.to_string()))
}

async fn build_transport(
    config: &RemoteServerConfig,
    client: reqwest::Client,
) -> Result<SseClientTransport<reqwest::Client>, RemoteProxyError> {
    let cfg = rmcp::transport::sse_client::SseClientConfig {
        sse_endpoint: config.url.clone().into(),
        ..Default::default()
    };

    SseClientTransport::start_with_client(client, cfg)
        .await
        .map_err(RemoteProxyError::Transport)
}

struct RemoteClientHandler {
    server_name: String,
}

#[derive(Clone, Debug)]
struct RetryPolicy {
    max_attempts: u32,
    initial_delay: Duration,
    max_delay: Duration,
    backoff_multiplier: f64,
}

impl RetryPolicy {
    fn from_config(config: Option<&RetryConfig>) -> Self {
        const DEFAULT_ATTEMPTS: u32 = 3;
        const DEFAULT_INITIAL_DELAY_MS: u64 = 500;
        const DEFAULT_MAX_DELAY_MS: u64 = 10_000;
        const DEFAULT_BACKOFF: f64 = 2.0;

        let (max_attempts, initial_delay_ms, max_delay_ms, backoff_multiplier) = config
            .map(|cfg| {
                (
                    cfg.max_attempts.unwrap_or(DEFAULT_ATTEMPTS).max(1),
                    cfg.initial_delay_ms.unwrap_or(DEFAULT_INITIAL_DELAY_MS),
                    cfg.max_delay_ms.unwrap_or(DEFAULT_MAX_DELAY_MS),
                    cfg.backoff_multiplier.unwrap_or(DEFAULT_BACKOFF),
                )
            })
            .unwrap_or((
                DEFAULT_ATTEMPTS,
                DEFAULT_INITIAL_DELAY_MS,
                DEFAULT_MAX_DELAY_MS,
                DEFAULT_BACKOFF,
            ));

        let initial_delay = Duration::from_millis(initial_delay_ms);
        let max_delay = Duration::from_millis(max_delay_ms.max(initial_delay_ms));
        let backoff_multiplier = if backoff_multiplier < 1.0 {
            1.0
        } else {
            backoff_multiplier
        };

        Self {
            max_attempts,
            initial_delay,
            max_delay,
            backoff_multiplier,
        }
    }

    async fn execute<F, Fut, T>(&self, mut operation: F) -> Result<T, RemoteProxyError>
    where
        F: FnMut(u32) -> Fut,
        Fut: Future<Output = Result<T, RemoteProxyError>> + Send,
    {
        let mut attempt: u32 = 0;
        let mut delay = self.initial_delay;

        loop {
            let result = operation(attempt).await;
            match result {
                Ok(value) => return Ok(value),
                Err(error) => {
                    if !error.is_retryable() {
                        return Err(error);
                    }

                    attempt += 1;
                    if attempt >= self.max_attempts {
                        debug!(attempt, error = ?error, "Retry attempts exhausted for remote operation");
                        return Err(error);
                    }

                    let sleep_duration = delay.min(self.max_delay);
                    if sleep_duration > Duration::ZERO {
                        sleep(sleep_duration).await;
                    }
                    let next_delay = delay.mul_f64(self.backoff_multiplier);
                    delay = next_delay.min(self.max_delay);
                }
            }
        }
    }
}

impl Service<RoleClient> for RemoteClientHandler {
    fn handle_request(
        &self,
        _request: ServerRequest,
        _context: RequestContext<RoleClient>,
    ) -> impl Future<Output = Result<ClientResult, McpError>> + Send + '_ {
        async {
            Err(McpError::internal_error(
                "Client does not handle requests",
                None,
            ))
        }
    }

    fn handle_notification(
        &self,
        notification: ServerNotification,
        _context: NotificationContext<RoleClient>,
    ) -> impl Future<Output = Result<(), McpError>> + Send + '_ {
        let server = self.server_name.clone();
        async move {
            match notification {
                ServerNotification::LoggingMessageNotification(log) => {
                    info!(target: "remote", server = %server, params = ?log.params, "Remote log message");
                }
                ServerNotification::ProgressNotification(progress) => {
                    info!(target: "remote", server = %server, params = ?progress.params, "Remote progress update");
                }
                other => {
                    info!(target: "remote", server = %server, ?other, "Remote notification");
                }
            }
            Ok(())
        }
    }

    fn get_info(&self) -> rmcp::model::ClientInfo {
        rmcp::model::ClientInfo::default()
    }
}

fn resolve_auth_headers(
    config: &RemoteServerConfig,
) -> Result<
    Option<Vec<(reqwest::header::HeaderName, reqwest::header::HeaderValue)>>,
    RemoteProxyError,
> {
    let auth = match &config.auth {
        Some(value) => value,
        None => return Ok(None),
    };

    match auth {
        AuthConfig::Bearer {
            token,
            token_env,
            header,
        } => {
            let token_value = token
                .clone()
                .or_else(|| token_env.as_ref().and_then(|key| env::var(key).ok()))
                .ok_or_else(|| {
                    RemoteProxyError::Auth(format!(
                        "Bearer auth for {} requires token or tokenEnv",
                        config.name
                    ))
                })?;

            let header_name = header.as_deref().unwrap_or("authorization");
            let header_name = reqwest::header::HeaderName::from_bytes(header_name.as_bytes())
                .map_err(|error| RemoteProxyError::Auth(error.to_string()))?;
            let header_value = if token_value.starts_with("Bearer ") {
                reqwest::header::HeaderValue::from_str(&token_value)
            } else {
                reqwest::header::HeaderValue::from_str(&format!("Bearer {token_value}"))
            }
            .map_err(|error| RemoteProxyError::Auth(error.to_string()))?;

            Ok(Some(vec![(header_name, header_value)]))
        }
        AuthConfig::Header {
            header,
            value,
            value_env,
        } => {
            let header_name = reqwest::header::HeaderName::from_bytes(header.as_bytes())
                .map_err(|error| RemoteProxyError::Auth(error.to_string()))?;
            let header_value = value
                .clone()
                .or_else(|| value_env.as_ref().and_then(|key| env::var(key).ok()))
                .ok_or_else(|| {
                    RemoteProxyError::Auth(format!(
                        "Header auth for {} requires value or valueEnv",
                        config.name
                    ))
                })?;
            let header_value = reqwest::header::HeaderValue::from_str(&header_value)
                .map_err(|error| RemoteProxyError::Auth(error.to_string()))?;
            Ok(Some(vec![(header_name, header_value)]))
        }
    }
}

#[derive(Debug)]
pub enum RemoteProxyError {
    Transport(rmcp::transport::sse_client::SseTransportError<reqwest::Error>),
    Service(ServiceError),
    ClientInit(service::ClientInitializeError),
    Config(String),
    Auth(String),
}

impl RemoteProxyError {
    fn into_mcp(self) -> McpError {
        match self {
            RemoteProxyError::Transport(error) => {
                McpError::internal_error(format!("Remote transport error: {error}"), None)
            }
            RemoteProxyError::Service(error) => {
                McpError::internal_error(format!("Remote service error: {error}"), None)
            }
            RemoteProxyError::ClientInit(error) => {
                McpError::internal_error(format!("Remote init failed: {error}"), None)
            }
            RemoteProxyError::Config(message) => {
                McpError::invalid_params(format!("Invalid remote configuration: {message}"), None)
            }
            RemoteProxyError::Auth(message) => {
                McpError::invalid_params(format!("Remote authentication error: {message}"), None)
            }
        }
    }

    fn is_retryable(&self) -> bool {
        match self {
            RemoteProxyError::Transport(_) => true,
            RemoteProxyError::Service(error) => matches!(
                error,
                ServiceError::TransportSend(_)
                    | ServiceError::TransportClosed
                    | ServiceError::Timeout { .. }
            ),
            RemoteProxyError::ClientInit(_) => true,
            RemoteProxyError::Config(_) | RemoteProxyError::Auth(_) => false,
        }
    }
}
