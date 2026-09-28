use super::*;
use rmcp::model::ContentBlock;

#[test]
fn extracts_function_and_custom_tool_outputs() {
    let request = json!({
        "input": [
            {"type":"function_call_output","call_id":"a","output":"one"},
            {"type":"custom_tool_call_output","call_id":"b","output":{"ok":true}},
            {"type":"message","role":"user","content":[]}
        ]
    });

    assert_eq!(
        extract_tool_outputs(&request),
        vec![
            ("a".to_string(), json!("one")),
            ("b".to_string(), json!({"ok":true})),
        ]
    );
}

#[test]
fn extracts_cua_preload_outputs() {
    let request = json!({
        "input": [
            {
                "type":"tool_search_output",
                "call_id":"search-1",
                "tools":[{"type":"function","name":"optional_tool"}]
            },
            {
                "type":"tool_search_output",
                "call_id":null,
                "tools":[]
            }
        ]
    });

    assert_eq!(
        extract_cua_preload_outputs(&request),
        vec![
            vec![json!({"type":"function","name":"optional_tool"})],
            Vec::new(),
        ]
    );
}

#[test]
fn skills_catalog_contains_only_compact_enabled_entries() {
    let full_description = "Complete skill guidance. ".repeat(32);
    let catalog = json!({
        "data": [{
            "cwd":"/private/workspace",
            "skills":[
                {
                    "name":"plugin:useful",
                    "description":full_description,
                    "path":"/private/skills/useful/SKILL.md",
                    "scope":"user",
                    "enabled":true,
                    "pluginId":"plugin@example"
                },
                {
                    "name":"plugin:disabled",
                    "description":"Disabled skill",
                    "path":"/private/skills/disabled/SKILL.md",
                    "scope":"user",
                    "enabled":false
                }
            ],
            "errors":[{"path":"/private/broken","message":"broken frontmatter"}]
        }]
    });

    assert_eq!(
        compact_skills_catalog(&catalog),
        json!({
            "skills":[{
                "name":"plugin:useful",
                "description":full_description,
                "scope":"user",
                "plugin_id":"plugin@example"
            }],
            "returned":1,
            "total":1,
            "truncated":false,
            "errors":["broken frontmatter"]
        })
    );
}

#[test]
fn publishes_available_native_tools_directly() {
    let registry = vec![
        json!({
            "type":"function",
            "name":"exec_command",
            "description":"Run a shell command.",
            "parameters":{
                "type":"object",
                "properties":{"cmd":{"type":"string"}},
                "required":["cmd"],
                "additionalProperties":false
            }
        }),
        json!({
            "type":"custom",
            "name":"apply_patch",
            "description":"Apply a patch to workspace files."
        }),
        json!({
            "type":"function",
            "name":"write_stdin",
            "description":"Write to a running command.",
            "parameters":{"type":"object","properties":{"session_id":{"type":"integer"}}}
        }),
        json!({
            "type":"function",
            "name":"view_image",
            "description":"View an image file.",
            "parameters":{"type":"object","properties":{"path":{"type":"string"}}}
        }),
        json!({
            "type":"namespace", "name":"mcp__cua_repl", "tools":[{
                "type":"function",
                "name":"js",
                "description":"Control available browser and computer surfaces.",
                "parameters":{
                    "type":"object",
                    "properties":{"code":{"type":"string"}},
                    "required":["code"],
                    "additionalProperties":false
                }
            }]
        }),
        json!({
            "type":"function",
            "name":"request_user_input",
            "description":"Ask the user a question.",
            "parameters":{"type":"object","properties":{}}
        }),
        json!({
            "type":"function",
            "name":"multi_agent_v1__spawn_agent",
            "description":"Spawn an agent.",
            "parameters":{"type":"object","properties":{}}
        }),
    ];
    let tools = BridgeMcpHandler::tools_for_registry(&registry);
    let names = tools
        .iter()
        .map(|tool| tool.name.as_ref())
        .collect::<Vec<_>>();

    assert_eq!(
        names,
        vec![
            "codex_skills_list",
            "codex_skill_get",
            "exec_command",
            "write_stdin",
            "apply_patch",
            "view_image",
            "mcp__cua_repl__js",
            "mcp__cua_repl__js_reset",
            "mcp__cua_repl__js_add_node_module_dir",
        ]
    );
    assert_eq!(
        tools
            .iter()
            .find(|tool| tool.name.as_ref() == "exec_command")
            .unwrap()
            .input_schema["required"],
        json!(["cmd"])
    );
    assert_eq!(
        tools
            .iter()
            .find(|tool| tool.name.as_ref() == "apply_patch")
            .unwrap()
            .input_schema["required"],
        json!(["input"])
    );
    assert_eq!(
        tools
            .iter()
            .find(|tool| tool.name.as_ref() == "mcp__cua_repl__js")
            .unwrap()
            .description
            .as_deref(),
        Some("Control available browser and computer surfaces.")
    );

    let without_cua = BridgeMcpHandler::tools_for_registry(&registry[..2]);
    for name in tool_registry::CUA_TOOL_NAMES {
        assert!(without_cua.iter().any(|tool| tool.name.as_ref() == name));
    }
}

#[test]
fn cua_fallback_tools_preserve_original_metadata() {
    let tools = BridgeMcpHandler::tools_for_registry(&[]);
    let js = tools
        .iter()
        .find(|tool| tool.name.as_ref() == "mcp__cua_repl__js")
        .unwrap();
    assert_eq!(
        js.description.as_deref(),
        Some(include_str!("cua_js_description_macos.md").trim_end())
    );
    assert_eq!(js.input_schema["required"], json!(["code"]));
    assert_eq!(
        js.input_schema["properties"]["timeout_ms"]["type"],
        json!("integer")
    );

    let reset = tools
        .iter()
        .find(|tool| tool.name.as_ref() == "mcp__cua_repl__js_reset")
        .unwrap();
    assert_eq!(
        reset.description.as_deref(),
        Some(
            "Reset the persistent cua_repl JavaScript session. All JavaScript bindings are discarded. The next cua_repl.js call initializes a fresh runtime for the enabled surfaces. This does not close browser tabs or native apps, or erase their state."
        )
    );
    assert_eq!(reset.input_schema["properties"], json!({}));

    let add_module_dir = tools
        .iter()
        .find(|tool| tool.name.as_ref() == "mcp__cua_repl__js_add_node_module_dir")
        .unwrap();
    assert_eq!(add_module_dir.input_schema["required"], json!(["path"]));
    assert_eq!(
        add_module_dir.input_schema["properties"]["path"]["type"],
        json!("string")
    );
}

#[test]
fn tool_execution_failures_are_visible_to_the_model() {
    let result = tool_execution_error("Chrome is not connected");

    assert_eq!(result.is_error, Some(true));
    assert_eq!(
        result.content,
        vec![ContentBlock::text("Chrome is not connected")]
    );
}

#[test]
fn unavailable_cua_tool_returns_stable_actionable_error() {
    for tool in tool_registry::CUA_TOOL_NAMES {
        let result = result_projection::cua_tool_unavailable(tool);

        assert_eq!(result.is_error, Some(true));
        assert_eq!(
            result.structured_content,
            Some(json!({
                "ok":false,
                "error":{
                    "code":"tool_unavailable",
                    "tool":tool,
                    "message":"Computer Use is currently unavailable.",
                    "requiredAction":"Open ChatGPT desktop app and make Computer Use available, then retry. If it remains unavailable, restart the connector.",
                    "retryable":true
                }
            }))
        );
    }
}

#[tokio::test]
async fn runtime_reset_preserves_stable_public_tool_list() {
    let bridge = Bridge::default();
    bridge.state.lock().await.tools = vec![json!({
        "type":"function",
        "name":"exec_command",
        "description":"Run a command.",
        "parameters":{"type":"object","properties":{}}
    })];

    let before = BridgeMcpHandler::tools_for_registry(&bridge.state.lock().await.tools)
        .into_iter()
        .map(|tool| tool.name.into_owned())
        .collect::<Vec<_>>();

    bridge.reset_runtime_registry().await;

    let after = BridgeMcpHandler::tools_for_registry(&bridge.state.lock().await.tools)
        .into_iter()
        .map(|tool| tool.name.into_owned())
        .collect::<Vec<_>>();

    assert_eq!(before, after);
    assert_eq!(
        before,
        vec![
            "codex_skills_list",
            "codex_skill_get",
            "exec_command",
            "mcp__cua_repl__js",
            "mcp__cua_repl__js_reset",
            "mcp__cua_repl__js_add_node_module_dir",
        ]
    );
}

#[tokio::test]
async fn runtime_reset_wakes_waiters_for_a_model_request() {
    let bridge = Bridge::default();
    let waiting_bridge = bridge.clone();
    let waiter = tokio::spawn(async move { waiting_bridge.take_model_response_sender().await });
    tokio::time::sleep(Duration::from_millis(10)).await;

    bridge.reset_runtime_registry().await;

    let result = tokio::time::timeout(Duration::from_millis(100), waiter)
        .await
        .expect("runtime reset should wake the waiter")
        .expect("waiter task should complete");
    assert!(result.is_err());
}

#[test]
fn codex_image_output_becomes_native_mcp_image_content() {
    let output = json!([
        {"type":"input_text","text":"before"},
        {
            "type":"input_image",
            "detail":"original",
            "image_url":"data:image/png;base64,AAAA"
        }
    ]);
    let result = codex_output_to_call_tool_result(output.clone());

    assert_eq!(result.structured_content, None);
    assert_eq!(result.content.len(), 2);
    assert_eq!(result.content[0], ContentBlock::text("before"));
    assert_eq!(result.content[1], ContentBlock::image("AAAA", "image/png"));
}

#[test]
fn malformed_image_output_keeps_the_structured_payload() {
    let output = json!([{
        "type":"input_image",
        "image_url":"https://example.com/not-embedded.png"
    }]);
    let result = codex_output_to_call_tool_result(output.clone());

    assert_eq!(
        result.structured_content,
        Some(json!({"output": output.clone()}))
    );
    assert_eq!(
        result.content,
        vec![ContentBlock::text(json!({"output": output}).to_string())]
    );
}

#[test]
fn mixed_output_preserves_native_content_and_unprojected_json() {
    let output = json!([
        {"type":"output_text","text":"useful text"},
        {"status":"completed","answer":42}
    ]);
    let result = codex_output_to_call_tool_result(output.clone());

    assert_eq!(result.content, vec![ContentBlock::text("useful text")]);
    assert_eq!(result.structured_content, Some(json!({"output": output})));
}

#[test]
fn plain_text_output_is_not_duplicated_as_structured_content() {
    let result = codex_output_to_call_tool_result(json!("command output"));

    assert_eq!(result.content, vec![ContentBlock::text("command output")]);
    assert_eq!(result.structured_content, None);
}

#[test]
fn mcp_server_instructions_include_bounded_local_codex_context() {
    let long_workspace = "я".repeat(server_context::MAX_SERVER_INSTRUCTIONS_BYTES);
    let bridge = Bridge::new(
        PathBuf::from(long_workspace),
        PathBuf::from("codex"),
        PathBuf::from("codex-home"),
        CodexAccessMode::DangerFullAccess,
    )
    .with_cua_tools();
    let instructions = BridgeMcpHandler { bridge }.get_info().instructions.unwrap();

    assert!(instructions.len() <= server_context::MAX_SERVER_INSTRUCTIONS_BYTES);
    assert!(instructions.is_char_boundary(instructions.len()));
    assert!(instructions.contains("At the start of each conversation"));
    assert!(instructions.contains("call `codex_skills_list` once"));
    assert!(instructions.contains("Browser Use, Computer Use, or any CUA tool"));
    assert!(instructions.contains("unless the user explicitly asks"));
    assert!(instructions.contains("Report the unavailable tool and `requiredAction`, if provided"));
    assert!(!instructions.contains("codex_tools_search"));
    assert!(!instructions.contains("codex_tool_call"));
    assert!(instructions.contains("danger-full-access"));
    assert!(!instructions.contains("computer_use="));
}

#[test]
fn server_context_is_mirrored_once_into_visible_tool_metadata() {
    let bridge = Bridge::new(
        PathBuf::from(r"C:\work\project"),
        PathBuf::from("codex"),
        PathBuf::from("codex-home"),
        CodexAccessMode::DangerFullAccess,
    );
    let instructions = bridge.server_instructions().unwrap();
    let tools = BridgeMcpHandler {
        bridge: bridge.clone(),
    }
    .tools_for_bridge(&[]);
    let skills_list = tools
        .iter()
        .find(|tool| tool.name == "codex_skills_list")
        .unwrap();
    let skill_get = tools
        .iter()
        .find(|tool| tool.name == "codex_skill_get")
        .unwrap();
    let skills_list_description = skills_list.description.as_deref().unwrap();
    let skill_get_description = skill_get.description.as_deref().unwrap();

    assert!(skills_list_description.contains("MCP server instructions (mirrored"));
    assert!(skills_list_description.contains(&instructions));
    assert!(!skill_get_description.contains("MCP server instructions"));
}
