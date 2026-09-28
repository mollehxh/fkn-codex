use rmcp::model::JsonObject;
use rmcp::model::Tool;
use serde_json::Value;
use serde_json::json;

pub(crate) const CUA_TOOL_NAMES: [&str; 3] = [
    "mcp__cua_repl__js",
    "mcp__cua_repl__js_reset",
    "mcp__cua_repl__js_add_node_module_dir",
];

const EXPOSED_NATIVE_TOOL_NAMES: [&str; 7] = [
    "exec_command",
    "write_stdin",
    "apply_patch",
    "view_image",
    "mcp__cua_repl__js",
    "mcp__cua_repl__js_reset",
    "mcp__cua_repl__js_add_node_module_dir",
];

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

pub(crate) fn exposed_native_tools(registry: &[Value]) -> Vec<Tool> {
    let available = direct_tools(registry);
    let fallbacks = cua_fallback_tools();
    EXPOSED_NATIVE_TOOL_NAMES
        .iter()
        .filter_map(|name| {
            available
                .iter()
                .find(|tool| tool.definition.name.as_ref() == *name)
                .map(|tool| tool.definition.clone())
                .or_else(|| {
                    fallbacks
                        .iter()
                        .find(|tool| tool.name.as_ref() == *name)
                        .cloned()
                })
        })
        .collect()
}

pub(crate) fn is_cua_tool(name: &str) -> bool {
    CUA_TOOL_NAMES.contains(&name)
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

fn cua_fallback_tools() -> Vec<Tool> {
    [
        (
            "mcp__cua_repl__js",
            include_str!("cua_js_description_macos.md").trim_end(),
            json!({
                "type":"object",
                "properties":{
                    "code":{
                        "type":"string",
                        "description":"JavaScript to execute using the initialized cua_repl runtime."
                    },
                    "timeout_ms":{
                        "type":"integer",
                        "description":"Optional execution timeout in milliseconds. Defaults to 30000 (30 seconds) when omitted."
                    },
                    "title":{
                        "type":"string",
                        "description":"Short user-facing description of what the code does."
                    }
                },
                "required":["code"],
                "additionalProperties":false
            }),
        ),
        (
            "mcp__cua_repl__js_reset",
            "Reset the persistent cua_repl JavaScript session. All JavaScript bindings are discarded. The next cua_repl.js call initializes a fresh runtime for the enabled surfaces. This does not close browser tabs or native apps, or erase their state.",
            json!({"type":"object","properties":{},"additionalProperties":false}),
        ),
        (
            "mcp__cua_repl__js_add_node_module_dir",
            "Add an absolute `node_modules` directory for package imports. The directory remains available after `js_reset`.",
            json!({
                "type":"object",
                "properties":{
                    "path":{
                        "type":"string",
                        "description":"Absolute path to a node_modules directory to add to Node package resolution."
                    }
                },
                "required":["path"],
                "additionalProperties":false
            }),
        ),
    ]
    .into_iter()
    .filter_map(|(name, description, schema)| {
        serde_json::from_value::<JsonObject>(schema)
            .ok()
            .map(|schema| Tool::new(name.to_string(), description.to_string(), schema))
    })
    .collect()
}
