use anyhow::Context as _;
use anyhow::Result;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;
use std::path::PathBuf;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

const REAL_CODEX_ENV: &str = "FKN_CODEX_REAL_BIN";
const AUTH_SOURCE_HOME_ENV: &str = "FKN_CODEX_AUTH_SOURCE_HOME";

#[derive(Debug, Deserialize)]
struct StoredAuth {
    auth_mode: Option<String>,
    tokens: Option<StoredTokens>,
}

#[derive(Debug, Deserialize)]
struct StoredTokens {
    access_token: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let real_codex = required_path_env(REAL_CODEX_ENV)?;
    let args = std::env::args_os().skip(1).collect::<Vec<_>>();

    if args.first().and_then(|arg| arg.to_str()) != Some("app-server") {
        return forward_process(real_codex, args).await;
    }

    proxy_app_server(real_codex, args).await
}

async fn proxy_app_server(real_codex: PathBuf, args: Vec<std::ffi::OsString>) -> Result<()> {
    let mut child = Command::new(&real_codex)
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| {
            format!(
                "start isolated Codex app-server at {}",
                real_codex.display()
            )
        })?;

    let mut child_stdin = child
        .stdin
        .take()
        .context("open isolated app-server stdin")?;
    let child_stdout = child
        .stdout
        .take()
        .context("open isolated app-server stdout")?;
    let mut child_lines = BufReader::new(child_stdout).lines();

    let stdin = tokio::io::stdin();
    let mut client_lines = BufReader::new(stdin).lines();
    let mut stdout = tokio::io::stdout();

    let mut client_closed = false;
    let mut child_closed = false;

    while !client_closed && !child_closed {
        tokio::select! {
            line = client_lines.next_line(), if !client_closed => {
                match line.context("read node_repl app-server request")? {
                    Some(line) => {
                        if let Some(response) = intercept_get_auth_status(&line)? {
                            stdout.write_all(response.as_bytes()).await?;
                            stdout.write_all(b"\n").await?;
                            stdout.flush().await?;
                        } else {
                            child_stdin.write_all(line.as_bytes()).await?;
                            child_stdin.write_all(b"\n").await?;
                            child_stdin.flush().await?;
                        }
                    }
                    None => {
                        client_closed = true;
                        let _ = child_stdin.shutdown().await;
                    }
                }
            }
            line = child_lines.next_line(), if !child_closed => {
                match line.context("read isolated Codex app-server response")? {
                    Some(line) => {
                        stdout.write_all(line.as_bytes()).await?;
                        stdout.write_all(b"\n").await?;
                        stdout.flush().await?;
                    }
                    None => child_closed = true,
                }
            }
        }
    }

    if !client_closed {
        while let Some(line) = child_lines.next_line().await? {
            stdout.write_all(line.as_bytes()).await?;
            stdout.write_all(b"\n").await?;
        }
        stdout.flush().await?;
    }

    let status = child
        .wait()
        .await
        .context("wait for isolated Codex app-server")?;
    if !status.success() && !client_closed {
        anyhow::bail!("isolated Codex app-server exited with {status}");
    }
    Ok(())
}

fn intercept_get_auth_status(line: &str) -> Result<Option<String>> {
    let Ok(request) = serde_json::from_str::<Value>(line) else {
        return Ok(None);
    };
    if request.get("method").and_then(Value::as_str) != Some("getAuthStatus") {
        return Ok(None);
    }
    let Some(id) = request.get("id").cloned() else {
        return Ok(None);
    };

    let include_token = request
        .get("params")
        .and_then(|params| params.get("includeToken"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let snapshot = load_read_only_chatgpt_auth()?;
    let result = json!({
        "authMethod": snapshot.auth_method,
        "authToken": if include_token { Some(snapshot.access_token) } else { None },
        "requiresOpenaiAuth": true,
    });
    Ok(Some(json!({"id": id, "result": result}).to_string()))
}

struct AuthSnapshot {
    auth_method: String,
    access_token: String,
}

fn load_read_only_chatgpt_auth() -> Result<AuthSnapshot> {
    let source_home = required_path_env(AUTH_SOURCE_HOME_ENV)?;
    let auth_path = source_home.join("auth.json");
    let bytes = std::fs::read(&auth_path)
        .with_context(|| format!("read Codex auth source {}", auth_path.display()))?;
    let stored: StoredAuth = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse Codex auth source {}", auth_path.display()))?;
    let auth_method = stored.auth_mode.unwrap_or_default();
    anyhow::ensure!(
        auth_method == "chatgpt",
        "Codex auth source is not a ChatGPT login"
    );
    let access_token = stored
        .tokens
        .map(|tokens| tokens.access_token)
        .filter(|token| !token.trim().is_empty())
        .context("Codex auth source has no ChatGPT access token")?;
    Ok(AuthSnapshot {
        auth_method,
        access_token,
    })
}

async fn forward_process(real_codex: PathBuf, args: Vec<std::ffi::OsString>) -> Result<()> {
    let status = Command::new(&real_codex)
        .args(args)
        .status()
        .await
        .with_context(|| format!("run bundled Codex at {}", real_codex.display()))?;
    if let Some(code) = status.code() {
        std::process::exit(code);
    }
    anyhow::bail!("bundled Codex terminated without an exit code")
}

fn required_path_env(name: &str) -> Result<PathBuf> {
    let value = std::env::var_os(name).with_context(|| format!("{name} is not set"))?;
    let path = PathBuf::from(value);
    anyhow::ensure!(
        path.exists(),
        "{name} path does not exist: {}",
        path.display()
    );
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn ignores_non_auth_requests() {
        let _guard = ENV_LOCK.lock().unwrap();
        assert!(
            intercept_get_auth_status(r#"{"id":1,"method":"account/read","params":{}}"#)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn auth_intercept_never_returns_refresh_token_or_id_token() {
        let _guard = ENV_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!("fkn-auth-shim-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("auth.json"),
            r#"{"auth_mode":"chatgpt","tokens":{"access_token":"access-secret","id_token":"id-secret","refresh_token":"refresh-secret","account_id":"account-secret"}}"#,
        )
        .unwrap();
        unsafe { std::env::set_var(AUTH_SOURCE_HOME_ENV, &root) };
        let response = intercept_get_auth_status(
            r#"{"id":7,"method":"getAuthStatus","params":{"includeToken":true,"refreshToken":false}}"#,
        )
        .unwrap()
        .unwrap();
        unsafe { std::env::remove_var(AUTH_SOURCE_HOME_ENV) };
        let _ = std::fs::remove_dir_all(&root);

        assert!(response.contains("access-secret"));
        assert!(!response.contains("id-secret"));
        assert!(!response.contains("refresh-secret"));
        assert!(!response.contains("account-secret"));
        let value: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(value["result"]["authMethod"], "chatgpt");
        assert_eq!(value["result"]["requiresOpenaiAuth"], true);
    }

    #[test]
    fn include_token_false_redacts_access_token() {
        let _guard = ENV_LOCK.lock().unwrap();
        let root =
            std::env::temp_dir().join(format!("fkn-auth-shim-redact-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("auth.json"),
            r#"{"auth_mode":"chatgpt","tokens":{"access_token":"access-secret"}}"#,
        )
        .unwrap();
        unsafe { std::env::set_var(AUTH_SOURCE_HOME_ENV, &root) };
        let response = intercept_get_auth_status(
            r#"{"id":8,"method":"getAuthStatus","params":{"includeToken":false,"refreshToken":false}}"#,
        )
        .unwrap()
        .unwrap();
        unsafe { std::env::remove_var(AUTH_SOURCE_HOME_ENV) };
        let _ = std::fs::remove_dir_all(&root);

        assert!(!response.contains("access-secret"));
        let value: Value = serde_json::from_str(&response).unwrap();
        assert!(value["result"]["authToken"].is_null());
    }
}
