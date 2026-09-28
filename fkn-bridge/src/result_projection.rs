use rmcp::model::CallToolResult;
use rmcp::model::ContentBlock;
use serde_json::Value;
use serde_json::json;

pub(crate) fn codex_output_to_call_tool_result(output: Value) -> CallToolResult {
    let mut content = Vec::new();
    if collect_mcp_content(&output, &mut content) {
        return CallToolResult::success(content);
    }
    let structured = if output.is_object() {
        output
    } else {
        json!({"output": output})
    };
    let mut result = CallToolResult::structured(structured);
    if !content.is_empty() {
        result.content = content;
    }
    result
}

pub(crate) fn tool_execution_error(error: impl std::fmt::Display) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(error.to_string())])
}

pub(crate) fn cua_tool_unavailable(tool: &str) -> CallToolResult {
    CallToolResult::structured_error(json!({
        "ok": false,
        "error": {
            "code": "tool_unavailable",
            "tool": tool,
            "message": "Computer Use is currently unavailable.",
            "requiredAction": "Open ChatGPT desktop app and make Computer Use available, then retry. If it remains unavailable, restart the connector.",
            "retryable": true
        }
    }))
}

fn collect_mcp_content(value: &Value, content: &mut Vec<ContentBlock>) -> bool {
    match value {
        Value::String(text) => {
            content.push(ContentBlock::text(text.clone()));
            true
        }
        Value::Array(items) => {
            let mut fully_projected = !items.is_empty();
            for item in items {
                fully_projected &= collect_mcp_content(item, content);
            }
            fully_projected
        }
        Value::Object(object) => match object.get("type").and_then(Value::as_str) {
            Some("input_text" | "output_text" | "text") => {
                if let Some(text) = object.get("text").and_then(Value::as_str) {
                    content.push(ContentBlock::text(text.to_string()));
                    return true;
                }
                false
            }
            Some("input_image" | "image") => {
                if let Some(image_url) = object.get("image_url").and_then(Value::as_str)
                    && let Some((mime_type, data)) = parse_base64_data_url(image_url)
                {
                    content.push(ContentBlock::image(data, mime_type));
                    return true;
                }
                false
            }
            _ => false,
        },
        _ => false,
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
