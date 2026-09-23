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
use rmcp::service::RequestContext;
use rmcp::service::RoleServer;
use rmcp::transport::StreamableHttpServerConfig;
use rmcp::transport::StreamableHttpService;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use serde_json::Value;
use serde_json::json;
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

#[derive(Debug)]
enum ModelReply {
    ToolCall {
        call_id: String,
        call_type: ToolCallType,
        namespace: Option<String>,
        name: String,
        payload: Value,
    },
    ToolSearch {
        call_id: String,
        query: String,
        limit: Option<u64>,
    },
    Finish {
        message: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ToolCallType {
    Function,
    Custom,
}

impl ToolCallType {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "function" => Some(Self::Function),
            "custom" => Some(Self::Custom),
            _ => None,
        }
    }
}

#[derive(Default)]
struct BridgeState {
    tools: Vec<Value>,
    pending_model_response: Option<oneshot::Sender<ModelReply>>,
    pending_tool_results: HashMap<String, oneshot::Sender<Value>>,
}

#[derive(Clone, Debug)]
struct CodexRuntime {
    workspace: PathBuf,
    codex_bin: PathBuf,
    codex_home: PathBuf,
}

#[derive(Clone)]
pub struct Bridge {
    state: Arc<Mutex<BridgeState>>,
    model_request_ready: Arc<Notify>,
    sequence: Arc<AtomicU64>,
    runtime: Option<Arc<CodexRuntime>>,
}

impl Default for Bridge {
    fn default() -> Self {
        Self {
            state: Arc::new(Mutex::new(BridgeState::default())),
            model_request_ready: Arc::new(Notify::new()),
            sequence: Arc::new(AtomicU64::new(0)),
            runtime: None,
        }
    }
}

impl Bridge {
    pub fn new(workspace: PathBuf, codex_bin: PathBuf, codex_home: PathBuf) -> Self {
        Self {
            runtime: Some(Arc::new(CodexRuntime {
                workspace,
                codex_bin,
                codex_home,
            })),
            ..Self::default()
        }
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
    pub async fn inventory(&self) -> Value {
        let state = self.state.lock().await;
        json!({
            "ready": state.pending_model_response.is_some(),
            "tools": state.tools,
        })
    }

    pub async fn is_ready(&self) -> bool {
        self.state.lock().await.pending_model_response.is_some()
    }

    async fn accept_model_request(&self, request: Value) -> Result<oneshot::Receiver<ModelReply>> {
        let outputs = extract_tool_outputs(&request);
        let search_outputs = extract_tool_search_outputs(&request);
        let advertised_tools = request.get("tools").and_then(Value::as_array).cloned();
        let (reply_tx, reply_rx) = oneshot::channel();
        let mut resolved = Vec::new();

        {
            let mut state = self.state.lock().await;
            if state.pending_model_response.is_some() {
                anyhow::bail!("Codex sent a second model request while one is still pending");
            }

            if let Some(tools) = advertised_tools {
                state.tools = tools;
            }

            for (call_id, output) in outputs {
                if let Some(sender) = state.pending_tool_results.remove(&call_id) {
                    resolved.push((sender, output));
                }
            }

            for (call_id, output, discovered_tools) in search_outputs {
                if let Some(sender) = state.pending_tool_results.remove(&call_id) {
                    resolved.push((sender, output));
                }
                merge_discovered_tools(&mut state.tools, discovered_tools);
            }

            state.pending_model_response = Some(reply_tx);
        }

        for (sender, output) in resolved {
            let _ = sender.send(output);
        }
        self.model_request_ready.notify_waiters();

        Ok(reply_rx)
    }

    async fn take_model_response_sender(&self) -> oneshot::Sender<ModelReply> {
        loop {
            let notified = self.model_request_ready.notified();
            if let Some(sender) = self.state.lock().await.pending_model_response.take() {
                return sender;
            }
            notified.await;
        }
    }

    pub async fn call_tool(
        &self,
        call_type: &str,
        namespace: Option<String>,
        name: String,
        payload: Value,
    ) -> Result<Value> {
        let call_type = ToolCallType::parse(call_type)
            .with_context(|| format!("unsupported Codex tool call type: {call_type}"))?;

        {
            let state = self.state.lock().await;
            if !registry_contains_tool(&state.tools, call_type, namespace.as_deref(), &name) {
                anyhow::bail!(
                    "tool is not present in the active Codex registry: type={call_type:?} namespace={namespace:?} name={name}"
                );
            }
        }

        let call_id = format!("fkn-call-{}", self.sequence.fetch_add(1, Ordering::Relaxed));
        let (result_tx, result_rx) = oneshot::channel();
        self.state
            .lock()
            .await
            .pending_tool_results
            .insert(call_id.clone(), result_tx);

        let model_sender = self.take_model_response_sender().await;
        if model_sender
            .send(ModelReply::ToolCall {
                call_id: call_id.clone(),
                call_type,
                namespace,
                name,
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

    pub async fn search_tools(&self, query: String, limit: Option<u64>) -> Result<Value> {
        let has_tool_search = self
            .state
            .lock()
            .await
            .tools
            .iter()
            .any(|tool| tool.get("type").and_then(Value::as_str) == Some("tool_search"));
        if !has_tool_search {
            anyhow::bail!("active Codex turn did not advertise tool_search");
        }

        let call_id = format!(
            "fkn-search-{}",
            self.sequence.fetch_add(1, Ordering::Relaxed)
        );
        let (result_tx, result_rx) = oneshot::channel();
        self.state
            .lock()
            .await
            .pending_tool_results
            .insert(call_id.clone(), result_tx);

        let model_sender = self.take_model_response_sender().await;
        if model_sender
            .send(ModelReply::ToolSearch {
                call_id: call_id.clone(),
                query,
                limit,
            })
            .is_err()
        {
            self.state
                .lock()
                .await
                .pending_tool_results
                .remove(&call_id);
            anyhow::bail!("active Codex model request closed before tool_search was delivered");
        }

        result_rx
            .await
            .context("Codex turn ended before returning tool_search output")
    }

    pub async fn finish(&self, message: String) -> Result<()> {
        let sender = self.take_model_response_sender().await;
        sender
            .send(ModelReply::Finish { message })
            .map_err(|_| anyhow::anyhow!("active Codex model request closed before finish"))
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

fn extract_tool_search_outputs(request: &Value) -> Vec<(String, Value, Vec<Value>)> {
    request
        .get("input")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            if item.get("type").and_then(Value::as_str) != Some("tool_search_output") {
                return None;
            }
            let call_id = item.get("call_id")?.as_str()?.to_string();
            let tools = item
                .get("tools")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            Some((call_id, item.clone(), tools))
        })
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
    collect_mcp_content(&output, &mut content);
    if content.is_empty() {
        content.push(ContentBlock::text(match &output {
            Value::String(text) => text.clone(),
            _ => output.to_string(),
        }));
    }

    let mut result = CallToolResult::success(content);
    result.structured_content = Some(json!({"output": output}));
    result
}

fn collect_mcp_content(value: &Value, content: &mut Vec<ContentBlock>) {
    match value {
        Value::String(text) => content.push(ContentBlock::text(text.clone())),
        Value::Array(items) => {
            for item in items {
                collect_mcp_content(item, content);
            }
        }
        Value::Object(object) => match object.get("type").and_then(Value::as_str) {
            Some("input_text" | "output_text" | "text") => {
                if let Some(text) = object.get("text").and_then(Value::as_str) {
                    content.push(ContentBlock::text(text.to_string()));
                }
            }
            Some("input_image" | "image") => {
                if let Some(image_url) = object.get("image_url").and_then(Value::as_str)
                    && let Some((mime_type, data)) = parse_base64_data_url(image_url)
                {
                    content.push(ContentBlock::image(data, mime_type));
                }
            }
            _ => {}
        },
        _ => {}
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

fn registry_contains_tool(
    tools: &[Value],
    call_type: ToolCallType,
    namespace: Option<&str>,
    name: &str,
) -> bool {
    tools.iter().any(|tool| {
        if registry_entry_matches(tool, call_type, namespace, name) {
            return true;
        }

        let parent_namespace = tool
            .get("name")
            .and_then(Value::as_str)
            .filter(|_| tool.get("type").and_then(Value::as_str) == Some("namespace"));
        if namespace.is_some() && parent_namespace != namespace {
            return false;
        }

        tool.get("tools")
            .and_then(Value::as_array)
            .is_some_and(|nested| {
                nested
                    .iter()
                    .any(|tool| registry_entry_matches(tool, call_type, None, name))
            })
    })
}

fn registry_entry_matches(
    tool: &Value,
    call_type: ToolCallType,
    namespace: Option<&str>,
    name: &str,
) -> bool {
    let expected_type = match call_type {
        ToolCallType::Function => "function",
        ToolCallType::Custom => "custom",
    };
    let type_matches = tool.get("type").and_then(Value::as_str) == Some(expected_type);
    let name_matches = tool.get("name").and_then(Value::as_str) == Some(name)
        || tool
            .get("function")
            .and_then(|function| function.get("name"))
            .and_then(Value::as_str)
            == Some(name);
    let namespace_matches = match namespace {
        Some(namespace) => {
            tool.get("namespace").and_then(Value::as_str) == Some(namespace)
                || tool.get("server_label").and_then(Value::as_str) == Some(namespace)
        }
        None => true,
    };

    type_matches && name_matches && namespace_matches
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
                ToolCallType::Function => json!({
                    "type": "function_call",
                    "call_id": call_id,
                    "name": name,
                    "arguments": serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string()),
                }),
                ToolCallType::Custom => json!({
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
            let mut arguments = json!({"query": query});
            if let Some(limit) = limit {
                arguments["limit"] = json!(limit);
            }
            events.push(json!({
                "type": "response.output_item.done",
                "item": {
                    "type": "tool_search_call",
                    "call_id": call_id,
                    "execution": "client",
                    "arguments": arguments,
                }
            }));
        }
        ModelReply::Finish { message } => {
            events.push(json!({
                "type": "response.output_item.done",
                "item": {
                    "type": "message",
                    "role": "assistant",
                    "id": format!("{response_id}-message"),
                    "content": [{"type": "output_text", "text": message}],
                },
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

    fn tools() -> Vec<Tool> {
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
            Self::tool(
                "codex_inventory",
                "Inspect the authoritative native tool registry advertised by the active hidden Codex runtime. Call this before the first codex_exec. Each returned entry preserves Codex's exact tool type, name, description, parameter/format schema, namespace metadata, and other native fields. Use those returned descriptions and schemas to choose and call tools; never invent a tool name or argument shape. Deferred plugin tools may require codex_search_tools before they appear in the callable registry.",
                json!({"type":"object","properties":{},"additionalProperties":false}),
                true,
            ),
            Self::tool(
                "codex_search_tools",
                "Discover deferred Codex tools that are not fully loaded in the initial inventory. Use this when you need a capability whose exact tool is not already visible, especially Browser, Chrome, native Computer Use, or plugin-provided MCP tools. Search by the capability you need. The response preserves Codex's exact namespace/tool descriptions and parameter schemas and also makes the returned tools immediately callable through codex_exec. Read that metadata before executing anything.",
                json!({
                    "type":"object",
                    "properties":{
                        "query":{
                            "type":"string",
                            "minLength":1,
                            "description":"Semantic capability query, such as 'browser chrome computer use screenshots click type text'. Prefer describing the desired capability rather than guessing an internal tool name."
                        },
                        "limit":{
                            "type":"integer",
                            "minimum":1,
                            "maximum":32,
                            "description":"Maximum number of deferred search matches to request. Omit unless the default search result set is insufficient."
                        }
                    },
                    "required":["query"],
                    "additionalProperties":false
                }),
                true,
            ),
            Self::tool(
                "codex_exec",
                "Execute exactly one native Codex tool that is already present in codex_inventory or was returned by codex_search_tools. Treat the native metadata as authoritative: copy the exact call type, namespace when present, tool name, and argument shape; do not guess unsupported fields. Use call_type='function' with arguments for JSON-schema function tools. Use call_type='custom' with input for freeform/custom tools such as apply_patch. Tool results, including text and images, are returned to you without asking hidden Codex to reason about the user's task. For Browser/Chrome screenshots, follow the bound tab documentation and emit the browser image with `await nodeRepl.emitImage(await tab.screenshot())`; do not substitute undocumented runtime helpers such as `rt.getScreenshot()` for a browser tab.",
                json!({
                    "type":"object",
                    "properties":{
                        "call_type":{
                            "type":"string",
                            "enum":["function","custom"],
                            "description":"Exact native call class. Use function for tools with a parameters schema and custom for freeform tools with a format definition."
                        },
                        "namespace":{
                            "type":"string",
                            "minLength":1,
                            "description":"Namespace returned by codex_search_tools for a namespaced/deferred tool, for example mcp__cua_repl. Omit for top-level native tools."
                        },
                        "name":{
                            "type":"string",
                            "minLength":1,
                            "description":"Exact native tool name from codex_inventory or codex_search_tools, for example exec_command, view_image, js, or apply_patch."
                        },
                        "arguments":{
                            "type":"object",
                            "additionalProperties":true,
                            "description":"Arguments for call_type=function. They must conform to the native tool's returned parameters schema."
                        },
                        "input":{
                            "type":"string",
                            "description":"Raw freeform input for call_type=custom. Do not JSON-wrap custom tool input."
                        }
                    },
                    "required":["call_type","name"],
                    "additionalProperties":false
                }),
                false,
            ),
            Self::tool(
                "codex_finish",
                "End the active hidden Codex execution turn only after the current controller task no longer needs any Codex tools. This terminates that hidden execution turn, so do not call it between related shell, Browser, Computer Use, or other tool operations that need to keep runtime state alive.",
                json!({
                    "type":"object",
                    "properties":{
                        "message":{
                            "type":"string",
                            "description":"Optional terminal message recorded as the hidden Codex turn's final assistant message. This is runtime bookkeeping, not the user-facing answer."
                        }
                    },
                    "additionalProperties":false
                }),
                false,
            ),
        ]
    }
}

impl ServerHandler for BridgeMcpHandler {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(Self::tools()))
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
                let output = self
                    .bridge
                    .skills_list(force_reload)
                    .await
                    .map_err(|error| McpError::internal_error(error.to_string(), None))?;
                Ok(CallToolResult::structured(output).into())
            }
            "codex_skill_get" => {
                let name = args
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|name| !name.trim().is_empty())
                    .ok_or_else(|| McpError::invalid_params("missing name", None))?;
                let output = self
                    .bridge
                    .skill_get(name)
                    .await
                    .map_err(|error| McpError::internal_error(error.to_string(), None))?;
                Ok(CallToolResult::structured(output).into())
            }
            "codex_inventory" => {
                Ok(CallToolResult::structured(self.bridge.inventory().await).into())
            }
            "codex_search_tools" => {
                let query = args
                    .get("query")
                    .and_then(Value::as_str)
                    .ok_or_else(|| McpError::invalid_params("missing query", None))?
                    .to_string();
                let limit = args.get("limit").and_then(Value::as_u64);
                let output = self
                    .bridge
                    .search_tools(query, limit)
                    .await
                    .map_err(|error| McpError::internal_error(error.to_string(), None))?;
                Ok(CallToolResult::structured(json!({"output": output})).into())
            }
            "codex_exec" => {
                let call_type = args
                    .get("call_type")
                    .and_then(Value::as_str)
                    .ok_or_else(|| McpError::invalid_params("missing call_type", None))?;
                let name = args
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| McpError::invalid_params("missing name", None))?
                    .to_string();
                let namespace = args
                    .get("namespace")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let payload = match call_type {
                    "function" => args.get("arguments").cloned().unwrap_or_else(|| json!({})),
                    "custom" => args
                        .get("input")
                        .cloned()
                        .unwrap_or(Value::String(String::new())),
                    other => {
                        return Err(McpError::invalid_params(
                            format!("unsupported call_type: {other}"),
                            None,
                        ));
                    }
                };

                let output = self
                    .bridge
                    .call_tool(call_type, namespace, name, payload)
                    .await
                    .map_err(|error| McpError::internal_error(error.to_string(), None))?;
                Ok(codex_output_to_call_tool_result(output).into())
            }
            "codex_finish" => {
                let message = args
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("External ChatGPT controller completed the Codex turn.")
                    .to_string();
                self.bridge
                    .finish(message)
                    .await
                    .map_err(|error| McpError::internal_error(error.to_string(), None))?;
                Ok(CallToolResult::structured(json!({"finished": true})).into())
            }
            other => Err(McpError::invalid_params(
                format!("unknown tool: {other}"),
                None,
            )),
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
