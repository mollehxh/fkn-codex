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
fn extracts_tool_search_output_and_discovered_tools() {
    let request = json!({
        "input": [{
            "type":"tool_search_output",
            "call_id":"search-1",
            "status":"completed",
            "execution":"client",
            "tools":[{
                "type":"namespace",
                "name":"browser",
                "description":"Browser automation namespace",
                "tools":[{
                    "type":"function",
                    "name":"open",
                    "description":"Open a browser page",
                    "parameters":{
                        "type":"object",
                        "properties":{"url":{"type":"string","description":"URL to open"}},
                        "required":["url"]
                    }
                }]
            }]
        }]
    });

    let outputs = extract_tool_search_outputs(&request);
    assert_eq!(outputs.len(), 1);
    let (call_id, raw_output, discovered) = &outputs[0];
    assert_eq!(call_id, "search-1");
    assert_eq!(raw_output, &request["input"][0]);
    assert_eq!(discovered, request["input"][0]["tools"].as_array().unwrap());
    assert_eq!(
        discovered[0]["tools"][0]["description"],
        "Open a browser page"
    );
    assert_eq!(
        discovered[0]["tools"][0]["parameters"]["properties"]["url"]["description"],
        "URL to open"
    );
}

#[test]
fn bridge_tool_catalog_has_actionable_descriptions_and_argument_docs() {
    let tools = BridgeMcpHandler::tools();
    assert_eq!(tools.len(), 6);

    for tool in &tools {
        let description = tool.description.as_deref().unwrap_or_default();
        assert!(
            description.len() >= 80,
            "tool {} has a weak description: {description:?}",
            tool.name
        );
    }

    let by_name = |name: &str| {
        tools
            .iter()
            .find(|tool| tool.name.as_ref() == name)
            .unwrap()
    };
    let search = by_name("codex_search_tools");
    assert!(search.description.as_deref().unwrap().contains("Browser"));
    assert!(
        search
            .description
            .as_deref()
            .unwrap()
            .contains("parameter schemas")
    );
    assert_eq!(
        search.input_schema["properties"]["query"]["minLength"],
        json!(1)
    );
    assert!(
        search.input_schema["properties"]["query"]["description"]
            .as_str()
            .is_some_and(|description| description.contains("Semantic capability"))
    );

    let exec = by_name("codex_exec");
    assert!(
        exec.description
            .as_deref()
            .unwrap()
            .contains("do not guess")
    );
    assert!(
        exec.description
            .as_deref()
            .unwrap()
            .contains("tab.screenshot()")
    );
    for property in ["call_type", "namespace", "name", "arguments", "input"] {
        assert!(
            exec.input_schema["properties"][property]["description"]
                .as_str()
                .is_some_and(|description| !description.is_empty()),
            "codex_exec.{property} is missing argument documentation"
        );
    }
}

#[test]
fn validates_function_and_custom_registry_entries() {
    let tools = vec![
        json!({"type":"function","name":"exec_command"}),
        json!({"type":"custom","name":"apply_patch"}),
        json!({
            "type":"namespace",
            "name":"mcp__node_repl",
            "tools":[{"type":"function","name":"js"}]
        }),
    ];

    assert!(registry_contains_tool(
        &tools,
        ToolCallType::Function,
        None,
        "exec_command"
    ));
    assert!(registry_contains_tool(
        &tools,
        ToolCallType::Custom,
        None,
        "apply_patch"
    ));
    assert!(!registry_contains_tool(
        &tools,
        ToolCallType::Function,
        None,
        "apply_patch"
    ));
    assert!(registry_contains_tool(
        &tools,
        ToolCallType::Function,
        Some("mcp__node_repl"),
        "js"
    ));
}

#[test]
fn discovered_namespace_replaces_stale_registry_entry() {
    let mut tools = vec![json!({
        "type":"namespace",
        "name":"mcp__example",
        "tools":[{"type":"function","name":"old"}]
    })];
    merge_discovered_tools(
        &mut tools,
        vec![json!({
            "type":"namespace",
            "name":"mcp__example",
            "tools":[{"type":"function","name":"new"}]
        })],
    );

    assert_eq!(tools.len(), 1);
    assert!(!registry_contains_tool(
        &tools,
        ToolCallType::Function,
        Some("mcp__example"),
        "old"
    ));
    assert!(registry_contains_tool(
        &tools,
        ToolCallType::Function,
        Some("mcp__example"),
        "new"
    ));
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

    assert_eq!(result.structured_content, Some(json!({"output": output})));
    assert_eq!(result.content.len(), 2);
    assert_eq!(result.content[0], ContentBlock::text("before"));
    assert_eq!(result.content[1], ContentBlock::image("AAAA", "image/png"));
}
