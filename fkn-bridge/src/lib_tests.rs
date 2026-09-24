use super::*;

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
    ];
    let tools = BridgeMcpHandler::tools_for_registry(&registry);
    let names = tools
        .iter()
        .map(|tool| tool.name.as_ref())
        .collect::<Vec<_>>();

    assert!(names.contains(&"codex_skills_list"));
    assert!(names.contains(&"codex_skill_get"));
    assert!(names.contains(&"exec_command"));
    assert!(names.contains(&"apply_patch"));
    assert!(names.contains(&"mcp__cua_repl__js"));
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
    assert!(
        tools
            .iter()
            .find(|tool| tool.name.as_ref() == "mcp__cua_repl__js")
            .unwrap()
            .description
            .as_deref()
            .is_some_and(|description| description.contains("iab) is unavailable"))
    );

    let without_cua = BridgeMcpHandler::tools_for_registry(&registry[..2]);
    assert!(
        without_cua
            .iter()
            .all(|tool| tool.name.as_ref() != "mcp__cua_repl__js")
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

#[tokio::test]
async fn runtime_reset_removes_stale_direct_tools() {
    let bridge = Bridge::default();
    bridge.state.lock().await.tools = vec![json!({
        "type":"function",
        "name":"exec_command",
        "description":"Run a command.",
        "parameters":{"type":"object","properties":{}}
    })];

    assert!(
        BridgeMcpHandler::tools_for_registry(&bridge.state.lock().await.tools)
            .iter()
            .any(|tool| tool.name.as_ref() == "exec_command")
    );

    bridge.reset_runtime_registry().await;

    assert!(
        BridgeMcpHandler::tools_for_registry(&bridge.state.lock().await.tools)
            .iter()
            .all(|tool| tool.name.as_ref() != "exec_command")
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

    assert_eq!(
        result.structured_content,
        Some(json!({"content_count":2,"image_count":1}))
    );
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

    assert_eq!(result.structured_content, Some(json!({"output": output})));
    assert_eq!(result.content, vec![ContentBlock::text(output.to_string())]);
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
    assert!(instructions.contains("Local Codex controller"));
    assert!(instructions.contains("access=danger-full-access"));
    assert!(instructions.contains("computer_use=enabled"));
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
