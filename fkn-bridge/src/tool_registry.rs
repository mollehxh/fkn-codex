use rmcp::model::JsonObject;
use rmcp::model::Tool;
use serde_json::Value;
use serde_json::json;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NativeToolCallType {
    Function,
    Custom,
}

#[derive(Clone, Debug)]
pub(crate) struct DirectTool {
    pub(crate) definition: Tool,
    pub(crate) call_type: NativeToolCallType,
    pub(crate) namespace: Option<String>,
    pub(crate) native_name: String,
}

impl DirectTool {
    pub(crate) fn payload(&self, arguments: JsonObject) -> Result<Value, String> {
        match self.call_type {
            NativeToolCallType::Function => Ok(Value::Object(arguments)),
            NativeToolCallType::Custom => arguments
                .get("input")
                .and_then(Value::as_str)
                .map(|input| Value::String(input.to_string()))
                .ok_or_else(|| "missing string argument: input".to_string()),
        }
    }

    pub(crate) fn same_target(&self, other: &Self) -> bool {
        self.call_type == other.call_type
            && self.namespace == other.namespace
            && self.native_name == other.native_name
    }
}

pub(crate) fn direct_tools(registry: &[Value]) -> Vec<DirectTool> {
    let mut tools = Vec::new();
    for entry in registry {
        if entry.get("type").and_then(Value::as_str) == Some("namespace") {
            let Some(namespace) = entry.get("name").and_then(Value::as_str) else {
                continue;
            };
            for nested in entry
                .get("tools")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(tool) = direct_tool(nested, Some(namespace)) {
                    tools.push(tool);
                }
            }
        } else if let Some(tool) = direct_tool(entry, None) {
            tools.push(tool);
        }
    }
    tools
}

pub(crate) fn find_direct_tool(registry: &[Value], public_name: &str) -> Option<DirectTool> {
    direct_tools(registry)
        .into_iter()
        .find(|tool| tool.definition.name.as_ref() == public_name)
}

fn direct_tool(entry: &Value, namespace: Option<&str>) -> Option<DirectTool> {
    let call_type = match entry.get("type").and_then(Value::as_str)? {
        "function" => NativeToolCallType::Function,
        "custom" => NativeToolCallType::Custom,
        _ => return None,
    };
    let metadata = entry.get("function").unwrap_or(entry);
    let native_name = metadata.get("name")?.as_str()?.to_string();
    let public_name = namespace
        .map(|namespace| format!("{namespace}__{native_name}"))
        .unwrap_or_else(|| native_name.clone());
    let description = metadata
        .get("description")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| format!("Execute the native Codex tool {public_name}."));
    let description = if namespace == Some("mcp__cua_repl") && native_name == "js" {
        "Execute JavaScript in the persistent Computer Use runtime for native apps and connected Chrome. The in-app browser (iab) is unavailable in this bridge; use Chrome for browser work. On the first call or after reset, execute exactly one entry point such as `await cua.getState()` or `let tab = await cua.createBrowserTab(\"chrome\", url, { sessionName: \"🔎 Task\" })`, then follow the returned documentation. Use nodeRepl.write for extra text and nodeRepl.emitImage for images."
            .to_string()
    } else {
        description
    };
    let schema = match call_type {
        NativeToolCallType::Function => metadata
            .get("parameters")
            .cloned()
            .unwrap_or_else(|| json!({"type":"object","properties":{}})),
        NativeToolCallType::Custom => json!({
            "type":"object",
            "properties":{
                "input":{
                    "type":"string",
                    "description":"Raw freeform input for this tool."
                }
            },
            "required":["input"],
            "additionalProperties":false
        }),
    };
    let input_schema = serde_json::from_value::<JsonObject>(schema).ok()?;

    Some(DirectTool {
        definition: Tool::new(public_name, description, input_schema),
        call_type,
        namespace: namespace.map(str::to_string),
        native_name,
    })
}
