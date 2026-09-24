mod server_context;
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
use rmcp::ErrorData as McpError;
use rmcp::Peer;
use rmcp::handler::server::ServerHandler;
use rmcp::model::CallToolRequestParams;
use rmcp::model::CallToolResult;
use rmcp::model::ContentBlock;
use rmcp::model::JsonObject;
use rmcp::model::ListToolsResult;
use rmcp::model::PaginatedRequestParams;
use rmcp::model::ServerCapabilities;
use rmcp::model::ServerInfo;
use rmcp::model::Tool;
use rmcp::model::ToolAnnotations;
use rmcp::service::NotificationContext;
use rmcp::service::RequestContext;
use rmcp::service::RoleServer;
use rmcp::transport::StreamableHttpServerConfig;
use rmcp::transport::StreamableHttpService;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use serde_json::Value;
use serde_json::json;
pub use server_context::CodexAccessMode;
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
use tool_registry::direct_tools;
use tool_registry::find_direct_tool;

#[derive(Debug)]
enum ModelReply {
    ToolCall {
        call_id: String,
        call_type: NativeToolCallType,
        namespace: Option<String>,
        name: String,
        payload: Value,
    },
    ToolSearch {
        call_id: String,
        query: String,
        limit: u64,
    },
}

#[derive(Default)]
struct BridgeState {
    tools: Vec<Value>,
    peers: Vec<Peer<RoleServer>>,
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
        let computer_use = if self.preload_cua_tools {
            server_context::ComputerUseStatus::Enabled
        } else {
            server_context::ComputerUseStatus::Disabled
        };
        server_context::render(&runtime.workspace, runtime.access_mode, computer_use)
    }

    pub async fn skills_list(&self, force_reload: bool) -> Result<Value> {
        let runtime = self
            .runtime
            .as_deref()
            .context("Codex runtime metadata is unavailable")?;
        native_skills_list(runtime, force_reload).await
    }

    pub async fn skill_get(&self, name: &str) -> Result<Value> {
        let catalog = self.skills_list(false).await?;
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
            "skill": skill,
            "content": content,
        }))
    }
    pub async fn is_ready(&self) -> bool {
        self.state.lock().await.pending_model_response.is_some()
    }

    async fn register_peer(&self, peer: Peer<RoleServer>) {
        let mut state = self.state.lock().await;
        state.peers.retain(|peer| !peer.is_transport_closed());
        state.peers.push(peer);
    }

    async fn notify_tool_list_changed(&self) {
        let peers = std::mem::take(&mut self.state.lock().await.peers);
        let mut active_peers = Vec::new();
        for peer in peers.into_iter().filter(|peer| !peer.is_transport_closed()) {
            match peer.notify_tool_list_changed().await {
                Ok(()) => active_peers.push(peer),
                Err(error) => {
                    eprintln!("[bridge] failed to notify MCP client about tool changes: {error}");
                }
            }
        }
        self.state.lock().await.peers.extend(active_peers);
    }

    pub async fn reset_runtime_registry(&self) {
        let changed = {
            let mut state = self.state.lock().await;
            let changed = !direct_tools(&state.tools).is_empty();
            state.tools.clear();
            state.pending_model_response.take();
            state.pending_tool_results.clear();
            state.cua_preload_completed = false;
            state.runtime_reset = true;
            changed
        };
        self.model_request_ready.notify_waiters();
        if changed {
            self.notify_tool_list_changed().await;
        }
    }

    async fn accept_model_request(&self, request: Value) -> Result<oneshot::Receiver<ModelReply>> {
        let outputs = extract_tool_outputs(&request);
        let search_outputs = extract_tool_search_outputs(&request);
        let advertised_tools = request.get("tools").and_then(Value::as_array).cloned();
        let (reply_tx, reply_rx) = oneshot::channel();
        let mut reply_tx = Some(reply_tx);
        let mut resolved = Vec::new();
        let registry_changed;
        let mut automatic_reply = None;

        {
            let mut state = self.state.lock().await;
            if state.pending_model_response.is_some() {
                anyhow::bail!("Codex sent a second model request while one is still pending");
            }
            state.runtime_reset = false;

            let previous_tools = direct_tools(&state.tools)
                .into_iter()
                .map(|tool| tool.definition)
                .collect::<Vec<_>>();
            if let Some(tools) = advertised_tools {
                state.tools = tools;
            }

            for (call_id, output) in outputs {
                if let Some(sender) = state.pending_tool_results.remove(&call_id) {
                    resolved.push((sender, output));
                }
            }

            for discovered_tools in search_outputs {
                merge_discovered_tools(&mut state.tools, discovered_tools);
            }

            let current_tools = direct_tools(&state.tools)
                .into_iter()
                .map(|tool| tool.definition)
                .collect::<Vec<_>>();
            registry_changed = previous_tools != current_tools;

            if self.preload_cua_tools && !state.cua_preload_completed {
                state.cua_preload_completed = true;
                if state
                    .tools
                    .iter()
                    .any(|tool| tool.get("type").and_then(Value::as_str) == Some("tool_search"))
                {
                    automatic_reply = Some(ModelReply::ToolSearch {
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
        if registry_changed {
            self.notify_tool_list_changed().await;
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

fn extract_tool_search_outputs(request: &Value) -> Vec<Vec<Value>> {
    request
        .get("input")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("tool_search_output"))
        .filter_map(|item| item.get("tools").and_then(Value::as_array).cloned())
        .collect()
}

fn merge_discovered_tools(registry: &mut Vec<Value>, discovered: Vec<Value>) {
    for tool in discovered {
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

fn codex_output_to_call_tool_result(output: Value) -> CallToolResult {
    let mut content = Vec::new();
    let image_count = collect_mcp_content(&output, &mut content);
    if content.is_empty() {
        content.push(ContentBlock::text(match &output {
            Value::String(text) => text.clone(),
            _ => output.to_string(),
        }));
    }

    let mut result = CallToolResult::success(content);
    result.structured_content = Some(if image_count == 0 {
        json!({"output": output})
    } else {
        json!({"content_count":result.content.len(),"image_count":image_count})
    });
    result
}

fn tool_execution_error(error: impl std::fmt::Display) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(error.to_string())])
}

fn collect_mcp_content(value: &Value, content: &mut Vec<ContentBlock>) -> usize {
    match value {
        Value::String(text) => {
            content.push(ContentBlock::text(text.clone()));
            0
        }
        Value::Array(items) => items
            .iter()
            .map(|item| collect_mcp_content(item, content))
            .sum(),
        Value::Object(object) => match object.get("type").and_then(Value::as_str) {
            Some("input_text" | "output_text" | "text") => {
                if let Some(text) = object.get("text").and_then(Value::as_str) {
                    content.push(ContentBlock::text(text.to_string()));
                }
                0
            }
            Some("input_image" | "image") => {
                if let Some(image_url) = object.get("image_url").and_then(Value::as_str)
                    && let Some((mime_type, data)) = parse_base64_data_url(image_url)
                {
                    content.push(ContentBlock::image(data, mime_type));
                    return 1;
                }
                0
            }
            _ => 0,
        },
        _ => 0,
    }
}

fn parse_base64_data_url(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("data:")?;
    let (metadata, data) = rest.split_once(',')?;
    let mime_type = metadata.strip_suffix(";base64")?;
    if mime_type.is_empty() || data.is_empty() {
        return None;
    }
    Some((mime_type.to_string(), data.to_string()))
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
        ModelReply::ToolSearch {
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
                "Discover the enabled Codex skills available for the configured workspace. Use this when the task may have a relevant Codex or plugin skill, or when you need to know the exact skill names before reading one. Results come from Codex's native skills/list catalog and include each skill's description, scope, plugin ID, enabled state, and canonical SKILL.md path. Do not guess skill names when this catalog can provide them.",
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
        for native in direct_tools(registry) {
            if tools.iter().all(|tool| tool.name != native.definition.name) {
                tools.push(native.definition);
            }
        }
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
        let info = ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_tool_list_changed()
                .build(),
        );
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

    async fn on_initialized(&self, context: NotificationContext<RoleServer>) {
        self.bridge.register_peer(context.peer).await;
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
                }
                .ok_or_else(|| McpError::invalid_params(format!("unknown tool: {name}"), None))?;
                let payload = tool
                    .payload(args)
                    .map_err(|error| McpError::invalid_params(error, None))?;
                match self.bridge.call_tool(tool, payload).await {
                    Ok(output) => Ok(codex_output_to_call_tool_result(output).into()),
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
