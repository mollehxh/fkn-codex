use anyhow::Context as _;
use anyhow::Result;
use clap::Parser;
use rmcp::ServiceExt as _;
use rmcp::model::CallToolRequestParams;
use rmcp::model::CallToolResult;
use rmcp::model::ContentBlock;
use rmcp::transport::StreamableHttpClientTransport;
use serde_json::Value;
use serde_json::json;
use std::path::PathBuf;

#[derive(Debug, Parser)]
struct Args {
    #[arg(long, default_value = "http://127.0.0.1:8787/mcp")]
    url: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let transport = StreamableHttpClientTransport::from_uri(args.url);
    let client = ().serve(transport).await?;

    let tools = client.list_tools(None).await?;
    let names = tools
        .tools
        .iter()
        .map(|tool| tool.name.as_ref())
        .collect::<Vec<_>>();
    println!("MCP tools: {}", names.join(", "));
    for expected in [
        "codex_skills_list",
        "codex_skill_get",
        "exec_command",
        "write_stdin",
        "apply_patch",
        "view_image",
        "mcp__cua_repl__js",
    ] {
        anyhow::ensure!(names.contains(&expected), "missing MCP tool {expected}");
    }
    let skills = client
        .call_tool(
            CallToolRequestParams::new("codex_skills_list").with_arguments(
                serde_json::from_value(json!({"force_reload": true}))
                    .context("build codex_skills_list arguments")?,
            ),
        )
        .await?
        .structured_content
        .context("codex_skills_list returned no structured content")?;
    let discovered_skills = skills
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .flat_map(|entry| {
            entry
                .get("skills")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
        })
        .collect::<Vec<_>>();
    for (name, plugin_id) in [
        ("browser:control-in-app-browser", "browser@openai-bundled"),
        ("chrome:control-chrome", "chrome@openai-bundled"),
    ] {
        anyhow::ensure!(
            discovered_skills.iter().any(|skill| {
                skill.get("name").and_then(Value::as_str) == Some(name)
                    && skill.get("pluginId").and_then(Value::as_str) == Some(plugin_id)
            }),
            "native plugin skill missing: {name} ({plugin_id})"
        );
    }
    let chrome_skill = client
        .call_tool(
            CallToolRequestParams::new("codex_skill_get").with_arguments(
                serde_json::from_value(json!({"name":"chrome:control-chrome"}))
                    .context("build codex_skill_get arguments")?,
            ),
        )
        .await?
        .structured_content
        .context("codex_skill_get returned no structured content")?;
    anyhow::ensure!(
        chrome_skill
            .get("content")
            .and_then(Value::as_str)
            .is_some_and(|content| !content.trim().is_empty()),
        "native Chrome plugin skill content is empty"
    );
    println!("PASS native Browser/Chrome plugin skills");

    let workspace = exec_text(
        &client,
        "exec_command",
        json!({"cmd":"pwd","yield_time_ms":1000,"max_output_tokens":1000}),
    )
    .await?;
    let workspace = parse_command_output(&workspace)
        .lines()
        .last()
        .context("pwd returned no path")?;
    let workspace = PathBuf::from(workspace);
    println!("Workspace: {}", workspace.display());

    let interactive = exec_text(
        &client,
        "exec_command",
        json!({
            "cmd": "printf 'READY\\n'; read x; printf 'GOT:%s\\n' \"$x\"",
            "tty": true,
            "yield_time_ms": 250,
            "max_output_tokens": 2000
        }),
    )
    .await?;
    let session_id = parse_session_id(&interactive)?;
    let stdin_result = exec_text(
        &client,
        "write_stdin",
        json!({
            "session_id": session_id,
            "chars": "hello\n",
            "yield_time_ms": 1000,
            "max_output_tokens": 2000
        }),
    )
    .await?;
    anyhow::ensure!(
        stdin_result.contains("GOT:hello"),
        "write_stdin did not reach exec session"
    );
    println!("PASS exec_command + write_stdin");

    let local_server = exec_text(
        &client,
        "exec_command",
        json!({
            "cmd": "python3 -m http.server 0 --bind 127.0.0.1",
            "tty": true,
            "yield_time_ms": 500,
            "max_output_tokens": 2000
        }),
    )
    .await?;
    anyhow::ensure!(
        local_server.contains("Serving HTTP on 127.0.0.1 port"),
        "workspace sandbox could not bind a local server: {local_server}"
    );
    let local_server_session = parse_session_id(&local_server)?;
    let stopped_server = exec_text(
        &client,
        "write_stdin",
        json!({
            "session_id": local_server_session,
            "chars": "\u{0003}",
            "yield_time_ms": 1000,
            "max_output_tokens": 2000
        }),
    )
    .await?;
    anyhow::ensure!(
        stopped_server.contains("KeyboardInterrupt") || stopped_server.contains("^C"),
        "local server session did not stop cleanly: {stopped_server}"
    );
    println!("PASS workspace-write local server binding");

    let suffix = std::process::id();
    let patch_name = format!("fkn-generic-smoke-{suffix}.txt");
    let image_name = format!("fkn-generic-smoke-{suffix}.png");
    let patch = format!(
        "*** Begin Patch\n*** Add File: {patch_name}\n+dynamic patch works\n*** End Patch\n"
    );
    let patch_result = exec_text(&client, "apply_patch", json!({"input":patch})).await?;
    anyhow::ensure!(
        patch_result.contains("Success"),
        "apply_patch failed: {patch_result}"
    );
    let verify = exec_text(
        &client,
        "exec_command",
        json!({"cmd":format!("cat {patch_name}"),"yield_time_ms":1000,"max_output_tokens":1000}),
    )
    .await?;
    anyhow::ensure!(
        verify.contains("dynamic patch works"),
        "patched file contents were wrong"
    );
    println!("PASS apply_patch");

    let png_base64 = "iVBORw0KGgoAAAANSUhEUgAAACAAAAAgCAIAAAD8GO2jAAAAKElEQVR4nO3NsQ0AAAzCMP5/un0CNkuZ41wybXsHAAAAAAAAAAAAxR4yw/wuPL6QkAAAAABJRU5ErkJggg==";
    let create_image = format!(
        "python3 -c 'import base64; open(\"{image_name}\",\"wb\").write(base64.b64decode(\"{png_base64}\"))'"
    );
    exec_text(
        &client,
        "exec_command",
        json!({"cmd":create_image,"yield_time_ms":1000,"max_output_tokens":1000}),
    )
    .await?;
    let image_result = exec(
        &client,
        "view_image",
        json!({"path":workspace.join(&image_name).to_string_lossy(),"detail":"original"}),
    )
    .await?;
    anyhow::ensure!(
        image_result
            .content
            .iter()
            .any(|content| matches!(content, ContentBlock::Image(image) if image.mime_type == "image/png")),
        "view_image did not return a native MCP image block"
    );
    println!("PASS view_image -> MCP image content");

    let cua = exec(
        &client,
        "mcp__cua_repl__js",
        json!({
            "code":"const browsers = await cua.listBrowsers({emit:false}); nodeRepl.write(browsers);",
            "timeout_ms":30000,
            "title":"Generic CUA smoke"
        }),
    )
    .await?;
    let browser_text = cua
        .content
        .iter()
        .filter_map(|content| match content {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    anyhow::ensure!(
        cua.is_error != Some(true),
        "Browser cua_repl call failed: {browser_text}"
    );
    anyhow::ensure!(
        browser_text.contains("Chrome"),
        "Browser cua_repl returned no browser backend: {browser_text}"
    );
    anyhow::ensure!(
        !browser_text.contains("family: 'iab'") && !browser_text.contains("name: 'IAB'"),
        "CUA advertised the unavailable iab backend: {browser_text}"
    );
    println!("PASS Browser through direct mcp__cua_repl__js");

    let bind_browser_tab = exec(
        &client,
        "mcp__cua_repl__js",
        json!({
            "code":"globalThis.fknSmokeTab = await cua.createBrowserTab(\"chrome\", \"about:blank\", { sessionName: \"🧪 FKN smoke\" });",
            "timeout_ms":30000,
            "title":"Open FKN screenshot smoke tab"
        }),
    )
    .await?;
    anyhow::ensure!(
        bind_browser_tab.is_error != Some(true),
        "failed to open Chrome smoke tab"
    );

    let browser_image = exec(
        &client,
        "mcp__cua_repl__js",
        json!({
            "code":"await nodeRepl.emitImage(await globalThis.fknSmokeTab.screenshot());",
            "timeout_ms":30000,
            "title":"Capture FKN browser screenshot"
        }),
    )
    .await?;
    anyhow::ensure!(
        browser_image.content.iter().any(
            |content| matches!(content, ContentBlock::Image(image) if image.mime_type.starts_with("image/"))
        ),
        "Browser screenshot did not return a native MCP image block"
    );
    println!("PASS Browser screenshot -> MCP image content");

    let _ = exec(
        &client,
        "mcp__cua_repl__js",
        json!({
            "code":"await globalThis.fknSmokeTab.close();",
            "timeout_ms":30000,
            "title":"Close FKN screenshot smoke tab"
        }),
    )
    .await?;

    let native_app = exec(
        &client,
        "mcp__cua_repl__js",
        json!({
            "code":"const apps = await cua.listApps({emit:false}); const chrome = apps.find(app => app.displayName === \"Google Chrome\" || app.id === \"com.google.Chrome\"); nodeRepl.write({nativeComputerUse:Boolean(chrome), chrome});",
            "timeout_ms":30000,
            "title":"Generic native Computer Use smoke"
        }),
    )
    .await?;
    let native_text = native_app
        .content
        .iter()
        .filter_map(|content| match content {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    anyhow::ensure!(
        native_text.contains("nativeComputerUse"),
        "native Computer Use did not return through cua_repl"
    );
    println!("PASS native Computer Use through direct mcp__cua_repl__js");

    let cua_generic = exec(
        &client,
        "mcp__cua_repl__js",
        json!({"code":"nodeRepl.write({genericDirect:true, answer:42});","timeout_ms":30000,"title":"Generic direct smoke"}),
    )
    .await?;
    let cua_generic_text = cua_generic
        .content
        .iter()
        .filter_map(|content| match content {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    anyhow::ensure!(
        cua_generic_text.contains("genericDirect") && cua_generic_text.contains("42"),
        "generic direct MCP output was not propagated"
    );
    println!("PASS generic namespaced MCP call through direct tool");

    let cleanup = format!(
        "python3 -c 'from pathlib import Path; [Path(p).unlink(missing_ok=True) for p in [\"{patch_name}\",\"{image_name}\"]]'"
    );
    let _ = exec_text(
        &client,
        "exec_command",
        json!({"cmd":cleanup,"yield_time_ms":1000,"max_output_tokens":1000}),
    )
    .await?;

    client.cancel().await?;
    println!("PASS: native Codex tools are exposed directly through MCP");
    Ok(())
}

async fn exec(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    name: &str,
    arguments: Value,
) -> Result<CallToolResult> {
    client
        .call_tool(CallToolRequestParams::new(name.to_string()).with_arguments(
            serde_json::from_value(arguments).with_context(|| format!("build {name} arguments"))?,
        ))
        .await
        .map_err(Into::into)
}

async fn exec_text(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    name: &str,
    arguments: Value,
) -> Result<String> {
    let result = exec(client, name, arguments).await?;
    result
        .structured_content
        .as_ref()
        .and_then(|value| value.get("output"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .with_context(|| format!("{name} output was not text"))
}

fn parse_session_id(output: &str) -> Result<u64> {
    let marker = "session ID ";
    let rest = output
        .split_once(marker)
        .map(|(_, rest)| rest)
        .context("exec_command did not return a session ID")?;
    let digits = rest
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>();
    digits.parse().context("parse exec session ID")
}

fn parse_command_output(output: &str) -> &str {
    output
        .split_once("Output:\n")
        .map(|(_, output)| output.trim())
        .unwrap_or(output.trim())
}
