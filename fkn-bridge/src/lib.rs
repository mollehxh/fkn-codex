mod result_projection;
mod server_context;
mod skill_catalog;
mod tool_registry;

use anyhow::Context as _;
use anyhow::Result;
use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::HeaderValue;
use axum::http::StatusCode;
use axum::http::header::CACHE_CONTROL;
use axum::http::header::CONTENT_TYPE;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::post;
use result_projection::codex_output_to_call_tool_result;
use result_projection::tool_execution_error;
use rmcp::ErrorData as McpError;
use rmcp::handler::server::ServerHandler;
use rmcp::model::CallToolRequestParams;
use rmcp::model::CallToolResult;
use rmcp::model::JsonObject;
use rmcp::model::ListToolsResult;
use rmcp::model::PaginatedRequestParams;
use rmcp::model::ServerCapabilities;
use rmcp::model::ServerInfo;
use rmcp::model::Tool;
use rmcp::model::ToolAnnotations;
use rmcp::service::RequestContext;
use rmcp::service::RoleServer;
use rmcp::transport::StreamableHttpServerConfig;
use rmcp::transport::StreamableHttpService;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use serde_json::Value;
use serde_json::json;
pub use server_context::CodexAccessMode;
use skill_catalog::compact_skill_metadata;
use skill_catalog::compact_skills_catalog;
use std::borrow::Cow;
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::io::AsyncBufReadExt as _;
use tokio::io::AsyncWriteExt as _;
use tokio::io::BufReader;
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio::sync::Notify;
use tokio::sync::oneshot;
use tool_registry::DirectTool;
use tool_registry::NativeToolCallType;
use tool_registry::exposed_native_tools;
use tool_registry::find_direct_tool;
use tool_registry::is_cua_tool;

#[derive(Debug)]
enum ModelReply {
    ToolCall {
        call_id: String,
        call_type: NativeToolCallType,
        namespace: Option<String>,
        name: String,
        payload: Value,
    },
    CuaPreload {
        call_id: String,
        query: String,
        limit: u64,
    },
}

#[derive(Default)]
struct BridgeState {
    tools: Vec<Value>,
    skills_catalog: Option<Value>,
    cua_preload_completed: bool,
    runtime_reset: bool,
    pending_model_response: Option<oneshot::Sender<ModelReply>>,
    pending_tool_results: HashMap<String, oneshot::Sender<Value>>,
}

#[derive(Clone, Debug)]
struct CodexRuntime {
    workspace: PathBuf,
    codex_bin: PathBuf,
    codex_home: PathBuf,
    access_mode: CodexAccessMode,
}

#[derive(Clone)]
pub struct Bridge {
    state: Arc<Mutex<BridgeState>>,
    model_request_ready: Arc<Notify>,
    sequence: Arc<AtomicU64>,
    runtime: Option<Arc<CodexRuntime>>,
    preload_cua_tools: bool,
}

impl Default for Bridge {
    fn default() -> Self {
        Self {
            state: Arc::new(Mutex::new(BridgeState::default())),
            model_request_ready: Arc::new(Notify::new()),
            sequence: Arc::new(AtomicU64::new(0)),
            runtime: None,
            preload_cua_tools: false,
        }
    }
}

impl Bridge {
    pub fn new(
        workspace: PathBuf,
        codex_bin: PathBuf,
        codex_home: PathBuf,
        access_mode: CodexAccessMode,
    ) -> Self {
        Self {
            runtime: Some(Arc::new(CodexRuntime {
                workspace,
                codex_bin,
                codex_home,
                access_mode,
            })),
            ..Self::default()
        }
    }

    pub fn with_cua_tools(mut self) -> Self {
        self.preload_cua_tools = true;
        self
    }

    fn server_instructions(&self) -> Option<String> {
        let runtime = self.runtime.as_deref()?;
        server_context::render(&runtime.workspace, runtime.access_mode)
    }

    pub async fn skills_list(&self, force_reload: bool) -> Result<Value> {
        let catalog = self.skills_catalog(force_reload).await?;
        Ok(compact_skills_catalog(&catalog))
    }

    pub async fn skill_get(&self, name: &str) -> Result<Value> {
        let catalog = self.skills_catalog(false).await?;
        let mut matches = Vec::new();
        for entry in catalog
            .get("data")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            for skill in entry
                .get("skills")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if skill.get("enabled").and_then(Value::as_bool) == Some(true)
                    && skill.get("name").and_then(Value::as_str) == Some(name)
                {
                    matches.push(skill.clone());
                }
            }
        }

        let skill = match matches.as_slice() {
            [] => anyhow::bail!("enabled Codex skill not found: {name}"),
            [skill] => skill.clone(),
            _ => {
                let candidates = matches
                    .iter()
                    .filter_map(|skill| skill.get("path").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join(", ");
                anyhow::bail!("Codex skill name is ambiguous: {name}; candidates: {candidates}");
            }
        };

        let path = skill
            .get("path")
            .and_then(Value::as_str)
            .context("Codex skill metadata did not include a path")?;
        let content =
            std::fs::read_to_string(path).with_context(|| format!("read Codex skill at {path}"))?;

        Ok(json!({
            "skill": compact_skill_metadata(&skill).context("invalid Codex skill metadata")?,
            "content": content,
        }))
    }

    async fn skills_catalog(&self, force_reload: bool) -> Result<Value> {
        if !force_reload && let Some(catalog) = self.state.lock().await.skills_catalog.clone() {
            return Ok(catalog);
        }

        let runtime = self
            .runtime
            .as_deref()
            .context("Codex runtime metadata is unavailable")?;
        let catalog = native_skills_list(runtime, force_reload).await?;
        self.state.lock().await.skills_catalog = Some(catalog.clone());
        Ok(catalog)
    }

    pub async fn is_ready(&self) -> bool {
        self.state.lock().await.pending_model_response.is_some()
    }

    pub async fn reset_runtime_registry(&self) {
        {
            let mut state = self.state.lock().await;
            state.pending_model_response.take();
            state.pending_tool_results.clear();
            state.cua_preload_completed = false;
            state.runtime_reset = true;
        }
        self.model_request_ready.notify_waiters();
    }

    async fn accept_model_request(&self, request: Value) -> Result<oneshot::Receiver<ModelReply>> {
        let outputs = extract_tool_outputs(&request);
        let preload_outputs = extract_cua_preload_outputs(&request);
        let advertised_tools = request.get("tools").and_then(Value::as_array).cloned();
        let (reply_tx, reply_rx) = oneshot::channel();
        let mut reply_tx = Some(reply_tx);
        let mut resolved = Vec::new();
        let mut automatic_reply = None;

        {
            let mut state = self.state.lock().await;
            if state.pending_model_response.is_some() {
                anyhow::bail!("Codex sent a second model request while one is still pending");
            }
            state.runtime_reset = false;

            if let Some(tools) = advertised_tools {
                state.tools = tools;
            }

            for (call_id, output) in outputs {
                if let Some(sender) = state.pending_tool_results.remove(&call_id) {
                    resolved.push((sender, output));
                }
            }

            for preloaded_tools in preload_outputs {
                merge_cua_preload_tools(&mut state.tools, preloaded_tools);
            }

            if self.preload_cua_tools && !state.cua_preload_completed {
                state.cua_preload_completed = true;
                if state
                    .tools
                    .iter()
                    .any(|tool| tool.get("type").and_then(Value::as_str) == Some("tool_search"))
                {
                    automatic_reply = Some(ModelReply::CuaPreload {
                        call_id: format!(
                            "fkn-preload-{}",
                            self.sequence.fetch_add(1, Ordering::Relaxed)
                        ),
                        query: "computer use cua repl browser chrome native apps".to_string(),
                        limit: 16,
                    });
                } else {
                    state.pending_model_response = reply_tx.take();
                }
            } else {
                state.pending_model_response = reply_tx.take();
            }
        }

        for (sender, output) in resolved {
            let _ = sender.send(output);
        }
        if let Some(reply) = automatic_reply {
            reply_tx
                .take()
                .expect("automatic preload retains the response sender")
                .send(reply)
                .map_err(|_| anyhow::anyhow!("failed to schedule optional Codex tool preload"))?;
        } else {
            self.model_request_ready.notify_waiters();
        }
        Ok(reply_rx)
    }

    async fn take_model_response_sender(&self) -> Result<oneshot::Sender<ModelReply>> {
        loop {
            let notified = self.model_request_ready.notified();
            let mut state = self.state.lock().await;
            if state.runtime_reset {
                anyhow::bail!("Codex runtime reset before a model request was available");
            }
            if let Some(sender) = state.pending_model_response.take() {
                return Ok(sender);
            }
            drop(state);
            notified.await;
        }
    }

    async fn call_tool(&self, tool: DirectTool, payload: Value) -> Result<Value> {
        {
            let state = self.state.lock().await;
            let current = find_direct_tool(&state.tools, tool.definition.name.as_ref())
                .context("tool is no longer present in the active Codex registry")?;
            anyhow::ensure!(
                current.same_target(&tool),
                "tool changed in the active Codex registry; refresh the MCP tool list"
            );
        }

        let call_id = format!("fkn-call-{}", self.sequence.fetch_add(1, Ordering::Relaxed));
        let (result_tx, result_rx) = oneshot::channel();
        self.state
            .lock()
            .await
            .pending_tool_results
            .insert(call_id.clone(), result_tx);

        let model_sender = self.take_model_response_sender().await?;
        if model_sender
            .send(ModelReply::ToolCall {
                call_id: call_id.clone(),
                call_type: tool.call_type,
                namespace: tool.namespace,
                name: tool.native_name,
                payload,
            })
            .is_err()
        {
            self.state
                .lock()
                .await
                .pending_tool_results
                .remove(&call_id);
            anyhow::bail!("active Codex model request closed before the tool call was delivered");
        }

        result_rx
            .await
            .context("Codex turn ended before returning the tool result")
    }
}

async fn native_skills_list(runtime: &CodexRuntime, force_reload: bool) -> Result<Value> {
    let mut child = Command::new(&runtime.codex_bin)
        .arg("app-server")
        .arg("--stdio")
        .env("CODEX_HOME", &runtime.codex_home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("spawn Codex app-server at {}", runtime.codex_bin.display()))?;

    let mut stdin = child.stdin.take().context("open Codex app-server stdin")?;
    let stdout = child
        .stdout
        .take()
        .context("open Codex app-server stdout")?;
    let mut stdout = BufReader::new(stdout);

    write_json_line(
        &mut stdin,
        &json!({
            "id": 1,
            "method": "initialize",
            "params": {
                "clientInfo": {
                    "name": "fkn-codex-bridge",
                    "title": "FKN Codex Bridge",
                    "version": env!("CARGO_PKG_VERSION")
                },
                "capabilities": {"experimentalApi": true}
            }
        }),
    )
    .await?;
    read_jsonrpc_result(&mut stdout, 1).await?;

    write_json_line(&mut stdin, &json!({"method": "initialized"})).await?;
    write_json_line(
        &mut stdin,
        &json!({
            "id": 2,
            "method": "skills/list",
            "params": {
                "cwds": [runtime.workspace],
                "forceReload": force_reload
            }
        }),
    )
    .await?;

    let result = read_jsonrpc_result(&mut stdout, 2).await;
    drop(stdin);
    let _ = child.kill().await;
    let _ = child.wait().await;
    result
}

async fn write_json_line(stdin: &mut tokio::process::ChildStdin, value: &Value) -> Result<()> {
    let payload = serde_json::to_vec(value)?;
    stdin.write_all(&payload).await?;
    stdin.write_all(b"\n").await?;
    stdin.flush().await?;
    Ok(())
}

async fn read_jsonrpc_result<R>(reader: &mut R, expected_id: i64) -> Result<Value>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let mut line = String::new();
            let bytes = reader.read_line(&mut line).await?;
            if bytes == 0 {
                anyhow::bail!("Codex app-server closed stdout before response {expected_id}");
            }
            let message: Value = serde_json::from_str(&line)
                .with_context(|| format!("parse Codex app-server JSON-RPC: {line:?}"))?;
            if message.get("id").and_then(Value::as_i64) != Some(expected_id) {
                continue;
            }
            if let Some(error) = message.get("error") {
                anyhow::bail!("Codex app-server request {expected_id} failed: {error}");
            }
            return message
                .get("result")
                .cloned()
                .context("Codex app-server response did not include result");
        }
    })
    .await
    .with_context(|| format!("timed out waiting for Codex app-server response {expected_id}"))?
}

fn extract_tool_outputs(request: &Value) -> Vec<(String, Value)> {
    request
        .get("input")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let item_type = item.get("type")?.as_str()?;
            if item_type != "function_call_output" && item_type != "custom_tool_call_output" {
                return None;
            }
            let call_id = item.get("call_id")?.as_str()?.to_string();
            let output = item.get("output").cloned().unwrap_or(Value::Null);
            Some((call_id, output))
        })
        .collect()
}

fn extract_cua_preload_outputs(request: &Value) -> Vec<Vec<Value>> {
    request
        .get("input")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("tool_search_output"))
        .filter_map(|item| item.get("tools").and_then(Value::as_array).cloned())
        .collect()
}

fn merge_cua_preload_tools(registry: &mut Vec<Value>, preloaded_tools: Vec<Value>) {
    for tool in preloaded_tools {
        let tool_type = tool.get("type").and_then(Value::as_str);
        let tool_name = tool.get("name").and_then(Value::as_str);
        let existing = registry.iter().position(|candidate| {
            candidate.get("type").and_then(Value::as_str) == tool_type
                && candidate.get("name").and_then(Value::as_str) == tool_name
        });
        if let Some(index) = existing {
            registry[index] = tool;
        } else {
            registry.push(tool);
        }
    }
}

async fn responses_handler(
    State(bridge): State<Bridge>,
    body: Bytes,
) -> Result<Response, StatusCode> {
    let request: Value = serde_json::from_slice(&body).map_err(|error| {
        eprintln!("[bridge] invalid /v1/responses JSON: {error}");
        StatusCode::BAD_REQUEST
    })?;
    let reply = bridge
        .accept_model_request(request)
        .await
        .map_err(|error| {
            eprintln!("[bridge] rejected Codex model request: {error:#}");
            StatusCode::CONFLICT
        })?
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let response_id = format!(
        "fkn-response-{}",
        bridge.sequence.fetch_add(1, Ordering::Relaxed)
    );
    Ok(sse_response(response_id, reply))
}

fn sse_response(response_id: String, reply: ModelReply) -> Response {
    let mut events = vec![json!({
        "type": "response.created",
        "response": {"id": response_id},
    })];

    match reply {
        ModelReply::ToolCall {
            call_id,
            call_type,
            namespace,
            name,
            payload,
        } => {
            let mut item = match call_type {
                NativeToolCallType::Function => json!({
                    "type": "function_call",
                    "call_id": call_id,
                    "name": name,
                    "arguments": serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string()),
                }),
                NativeToolCallType::Custom => json!({
                    "type": "custom_tool_call",
                    "call_id": call_id,
                    "name": name,
                    "input": payload.as_str().map(str::to_string).unwrap_or_else(|| payload.to_string()),
                }),
            };
            if let Some(namespace) = namespace {
                item["namespace"] = Value::String(namespace);
            }
            events.push(json!({
                "type": "response.output_item.done",
                "item": item,
            }));
        }
        ModelReply::CuaPreload {
            call_id,
            query,
            limit,
        } => {
            events.push(json!({
                "type": "response.output_item.done",
                "item": {
                    "type": "tool_search_call",
                    "call_id": call_id,
                    "execution": "client",
                    "arguments": {"query": query, "limit": limit},
                }
            }));
        }
    }

    events.push(json!({
        "type": "response.completed",
        "response": {
            "id": response_id,
            "usage": {
                "input_tokens": 0,
                "input_tokens_details": null,
                "output_tokens": 0,
                "output_tokens_details": null,
                "total_tokens": 0
            }
        }
    }));

    let mut body = String::new();
    for event in events {
        let event_type = event["type"].as_str().unwrap_or("message");
        body.push_str("event: ");
        body.push_str(event_type);
        body.push('\n');
        body.push_str("data: ");
        body.push_str(&event.to_string());
        body.push_str("\n\n");
    }

    let mut response = body.into_response();
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response
}

#[derive(Clone)]
struct BridgeMcpHandler {
    bridge: Bridge,
}

impl BridgeMcpHandler {
    fn tool(name: &'static str, description: &'static str, schema: Value, read_only: bool) -> Tool {
        let schema = serde_json::from_value::<JsonObject>(schema).unwrap_or_default();
        let mut tool = Tool::new(
            Cow::Borrowed(name),
            Cow::Borrowed(description),
            Arc::new(schema),
        );
        if read_only {
            tool.annotations = Some(ToolAnnotations::new().read_only(true));
        }
        tool
    }

    fn controller_tools() -> Vec<Tool> {
        vec![
            Self::tool(
                "codex_skills_list",
                "List the enabled Codex skills available for this workspace. Call this exactly once on first use of this MCP server in a conversation, then read only relevant skills with codex_skill_get. The result is a compact catalog of exact skill names, descriptions, scopes, and plugin IDs; local paths and disabled skills are omitted. Repeat only when skills or plugins may have changed.",
                json!({
                    "type":"object",
                    "properties":{
                        "force_reload":{
                            "type":"boolean",
                            "description":"Reload Codex's skill catalog from its native sources instead of using the current catalog cache. Use when plugins or skills may have changed during this session. Defaults to false."
                        }
                    },
                    "additionalProperties":false
                }),
                true,
            ),
            Self::tool(
                "codex_skill_get",
                "Read the complete instructions for one enabled Codex skill so you can follow that skill yourself. Call codex_skills_list first when the exact skill name is unknown. This resolves the name through Codex's native catalog and returns the canonical SKILL.md; it does not accept arbitrary filesystem paths and it does not ask hidden Codex to reason about the user's task.",
                json!({
                    "type":"object",
                    "properties":{
                        "name":{
                            "type":"string",
                            "minLength":1,
                            "description":"Exact enabled skill name returned by codex_skills_list, for example chrome:control-chrome."
                        }
                    },
                    "required":["name"],
                    "additionalProperties":false
                }),
                true,
            ),
        ]
    }

    fn tools_for_registry(registry: &[Value]) -> Vec<Tool> {
        let mut tools = Self::controller_tools();
        tools.extend(exposed_native_tools(registry));
        tools
    }

    fn tools_for_bridge(&self, registry: &[Value]) -> Vec<Tool> {
        let mut tools = Self::tools_for_registry(registry);
        if let Some(instructions) = self.bridge.server_instructions()
            && let Some(tool) = tools
                .iter_mut()
                .find(|tool| tool.name == "codex_skills_list")
        {
            let description = tool.description.as_deref().unwrap_or_default();
            tool.description = Some(Cow::Owned(format!(
                "MCP server instructions (mirrored for clients that omit InitializeResult.instructions): {instructions}\n\n{}",
                description
            )));
        }
        tools
    }
}

impl ServerHandler for BridgeMcpHandler {
    fn get_info(&self) -> ServerInfo {
        let info = ServerInfo::new(ServerCapabilities::builder().enable_tools().build());
        match self.bridge.server_instructions() {
            Some(instructions) => info.with_instructions(instructions),
            None => info,
        }
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let registry = self.bridge.state.lock().await.tools.clone();
        Ok(ListToolsResult::with_all_items(
            self.tools_for_bridge(&registry),
        ))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<rmcp::model::CallToolResponse, McpError> {
        let args = request.arguments.unwrap_or_default();
        match request.name.as_ref() {
            "codex_skills_list" => {
                let force_reload = args
                    .get("force_reload")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                match self.bridge.skills_list(force_reload).await {
                    Ok(output) => Ok(CallToolResult::structured(output).into()),
                    Err(error) => Ok(tool_execution_error(error).into()),
                }
            }
            "codex_skill_get" => {
                let name = args
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|name| !name.trim().is_empty())
                    .ok_or_else(|| McpError::invalid_params("missing name", None))?;
                match self.bridge.skill_get(name).await {
                    Ok(output) => Ok(CallToolResult::structured(output).into()),
                    Err(error) => Ok(tool_execution_error(error).into()),
                }
            }
            name => {
                let tool = {
                    let state = self.bridge.state.lock().await;
                    find_direct_tool(&state.tools, name)
                };
                let Some(tool) = tool else {
                    if is_cua_tool(name) {
                        return Ok(result_projection::cua_tool_unavailable(name).into());
                    }
                    return Err(McpError::invalid_params(
                        format!("unknown tool: {name}"),
                        None,
                    ));
                };
                let payload = tool
                    .payload(args)
                    .map_err(|error| McpError::invalid_params(error, None))?;
                match self.bridge.call_tool(tool, payload).await {
                    Ok(output) => Ok(codex_output_to_call_tool_result(output).into()),
                    Err(error) if is_cua_tool(name) => {
                        eprintln!("[bridge] CUA tool {name} is unavailable: {error:#}");
                        Ok(result_projection::cua_tool_unavailable(name).into())
                    }
                    Err(error) => Ok(tool_execution_error(error).into()),
                }
            }
        }
    }
}

pub fn build_router(bridge: Bridge) -> Router {
    let mcp_bridge = bridge.clone();
    let mcp_service = StreamableHttpService::new(
        move || {
            Ok(BridgeMcpHandler {
                bridge: mcp_bridge.clone(),
            })
        },
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    );

    Router::new()
        .route("/v1/responses", post(responses_handler))
        .nest_service("/mcp", mcp_service)
        .with_state(bridge)
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
