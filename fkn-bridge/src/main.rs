mod desktop_runtime;

use anyhow::Context as _;
use anyhow::Result;
use clap::Parser;
use clap::ValueEnum;
use desktop_runtime::DesktopRuntimeLocator;
use fkn_codex_bridge::Bridge;
use fkn_codex_bridge::build_router;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Stdio;
use tokio::net::TcpListener;
use tokio::process::Command;

#[derive(Debug, Parser)]
struct Args {
    #[arg(long)]
    workspace: PathBuf,

    #[arg(long)]
    codex_bin: PathBuf,

    #[arg(long)]
    codex_home: PathBuf,

    /// Auth shim used only by Browser/Computer Use's nested Codex app-server.
    #[arg(long)]
    auth_shim_bin: Option<PathBuf>,

    #[arg(long, default_value = "127.0.0.1:8787")]
    listen: SocketAddr,

    #[arg(long, default_value = "gpt-5.4")]
    model: String,

    #[arg(long, value_enum, default_value_t = SandboxMode::WorkspaceWrite)]
    sandbox: SandboxMode,

    /// Write the bound local bridge URLs as JSON after the listener is ready.
    #[arg(long)]
    ready_file: Option<PathBuf>,

    /// Override the ChatGPT Desktop install used for Browser / Computer Use.
    #[arg(long)]
    chatgpt_app: Option<PathBuf>,

    /// Start hidden Codex without Browser / Computer Use support.
    #[arg(long, default_value_t = false)]
    disable_cua: bool,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum SandboxMode {
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
}

impl SandboxMode {
    fn as_codex_value(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
            Self::DangerFullAccess => "danger-full-access",
        }
    }
}

const FKN_WORKSPACE_PERMISSION_PROFILE: &str = r#"permissions.fkn-workspace={ extends=":workspace", network={ enabled=true, mode="limited", allow_local_binding=true } }"#;

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = Args::parse();
    std::fs::create_dir_all(&args.codex_home)
        .with_context(|| format!("create CODEX_HOME {}", args.codex_home.display()))?;
    args.workspace = std::fs::canonicalize(&args.workspace)
        .with_context(|| format!("resolve workspace {}", args.workspace.display()))?;
    args.codex_home = std::fs::canonicalize(&args.codex_home)
        .with_context(|| format!("resolve CODEX_HOME {}", args.codex_home.display()))?;
    args.codex_bin = std::fs::canonicalize(&args.codex_bin)
        .with_context(|| format!("resolve Codex binary {}", args.codex_bin.display()))?;

    if let Some(path) = &args.auth_shim_bin {
        args.auth_shim_bin = Some(
            std::fs::canonicalize(path)
                .with_context(|| format!("resolve FKN auth shim {}", path.display()))?,
        );
    }

    let desktop_runtime = if args.disable_cua {
        None
    } else {
        Some(DesktopRuntimeLocator::locate(args.chatgpt_app.as_deref())?)
    };

    let auth_shim_bin = if desktop_runtime.is_some() {
        Some(resolve_auth_shim(args.auth_shim_bin.as_deref())?)
    } else {
        None
    };
    let auth_source_home = if desktop_runtime.is_some() {
        Some(resolve_auth_source_home(&args.codex_home)?)
    } else {
        None
    };

    if let Some(runtime) = &desktop_runtime {
        runtime
            .bootstrap_native_plugins(&args.codex_home, &args.codex_bin)
            .await?;
    }

    let listener = TcpListener::bind(args.listen).await?;
    let address = listener.local_addr()?;
    let base_url = format!("http://{address}");
    let bridge = Bridge::new(
        args.workspace.clone(),
        args.codex_bin.clone(),
        args.codex_home.clone(),
    );
    let router = build_router(bridge.clone());

    println!("FKN Codex bridge");
    println!("MCP:       {base_url}/mcp");
    println!("Responses: {base_url}/v1/responses");
    println!("Workspace: {}", args.workspace.display());
    println!("CODEX_HOME: {}", args.codex_home.display());
    match &desktop_runtime {
        Some(runtime) => {
            println!("Desktop:   {}", runtime.install_root().display());
            println!(
                "CUA:       enabled (ChatGPT runtime {}, native plugins + browser+computer)",
                runtime.app_version()
            );
        }
        None => println!("CUA:       disabled"),
    }

    let mut server = tokio::spawn(async move { axum::serve(listener, router).await });

    let provider = format!(
        "model_providers.fkn={{ name = \"fkn-bridge\", base_url = \"{base_url}/v1\", env_key = \"FKN_CODEX_BRIDGE_KEY\", wire_api = \"responses\" }}"
    );
    let mut codex = Command::new(&args.codex_bin);
    codex
        .arg("exec")
        .arg("--skip-git-repo-check")
        .arg("--model")
        .arg(&args.model)
        .arg("-c")
        .arg(provider)
        .arg("-c")
        .arg("model_provider=\"fkn\"")
        .arg("-C")
        .arg(&args.workspace)
        .arg("External ChatGPT controller session. Execute only tool calls supplied by the configured model provider.")
        .env("CODEX_HOME", &args.codex_home)
        .env("FKN_CODEX_BRIDGE_KEY", "dummy")
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    match args.sandbox {
        SandboxMode::WorkspaceWrite => {
            codex
                .arg("-c")
                .arg("default_permissions=\"fkn-workspace\"")
                .arg("-c")
                .arg(FKN_WORKSPACE_PERMISSION_PROFILE);
        }
        SandboxMode::ReadOnly | SandboxMode::DangerFullAccess => {
            codex.arg("--sandbox").arg(args.sandbox.as_codex_value());
        }
    }

    if let Some(runtime) = &desktop_runtime {
        runtime.apply_cua_config(
            &mut codex,
            &args.codex_home,
            &args.codex_bin,
            auth_shim_bin.as_deref().expect("CUA auth shim resolved"),
            auth_source_home
                .as_deref()
                .expect("CUA auth source home resolved"),
        );
    }

    let mut child = codex.spawn().context("spawn bundled Codex")?;

    if let Some(path) = &args.ready_file {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            if bridge.is_ready().await {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).with_context(|| {
                        format!("create bridge ready-file directory {}", parent.display())
                    })?;
                }
                let payload = serde_json::to_vec_pretty(&serde_json::json!({
                    "mcp_url": format!("{base_url}/mcp"),
                    "responses_url": format!("{base_url}/v1/responses"),
                    "listen": address.to_string(),
                }))?;
                let temp = path.with_extension("tmp");
                std::fs::write(&temp, payload)
                    .with_context(|| format!("write bridge ready file {}", temp.display()))?;
                std::fs::rename(&temp, path)
                    .with_context(|| format!("publish bridge ready file {}", path.display()))?;
                break;
            }
            if let Some(status) = child.try_wait().context("poll bundled Codex")? {
                anyhow::bail!("bundled Codex exited during startup with {status}");
            }
            anyhow::ensure!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for hidden Codex to advertise its tool registry"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    loop {
        tokio::select! {
            status = child.wait() => {
                let status = status.context("wait for bundled Codex")?;
                if !status.success() {
                    anyhow::bail!("bundled Codex exited with {status}");
                }
                child = codex.spawn().context("restart bundled Codex after completed turn")?;
            }
            result = &mut server => {
                result.context("join bridge HTTP server")??;
                break;
            }
            result = tokio::signal::ctrl_c() => {
                result.context("listen for Ctrl-C")?;
                let _ = child.kill().await;
                break;
            }
        }
    }

    Ok(())
}

fn resolve_auth_shim(explicit: Option<&std::path::Path>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return Ok(path.to_path_buf());
    }
    let exe = std::env::current_exe().context("resolve current bridge executable")?;
    let directory = exe
        .parent()
        .context("bridge executable has no parent directory")?;
    let name = if cfg!(windows) {
        "fkn-codex-auth-shim.exe"
    } else {
        "fkn-codex-auth-shim"
    };
    let path = directory.join(name);
    anyhow::ensure!(
        path.is_file(),
        "FKN Browser auth shim was not found at {}; build/package fkn-codex-auth-shim beside fkn-codex-bridge or pass --auth-shim-bin",
        path.display()
    );
    std::fs::canonicalize(&path)
        .with_context(|| format!("resolve FKN Browser auth shim {}", path.display()))
}

fn resolve_auth_source_home(isolated_home: &std::path::Path) -> Result<PathBuf> {
    let candidate = std::env::var_os("FKN_CODEX_AUTH_SOURCE_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or(default_user_codex_home()?);
    anyhow::ensure!(
        candidate != isolated_home,
        "Browser auth source must be separate from the isolated FKN CODEX_HOME"
    );
    anyhow::ensure!(
        candidate.join("auth.json").is_file(),
        "ChatGPT/Codex auth source is unavailable at {}; sign in with the normal Codex runtime or set FKN_CODEX_AUTH_SOURCE_HOME",
        candidate.display()
    );
    std::fs::canonicalize(&candidate)
        .with_context(|| format!("resolve Codex auth source home {}", candidate.display()))
}

fn default_user_codex_home() -> Result<PathBuf> {
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .filter(|value| !value.is_empty())
        .context("user home directory is unavailable")?;
    Ok(PathBuf::from(home).join(".codex"))
}
