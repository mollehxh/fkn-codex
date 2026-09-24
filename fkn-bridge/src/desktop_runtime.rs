use anyhow::Context as _;
use anyhow::Result;
use serde_json::Value;
use std::ffi::OsStr;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use tokio::process::Command;

const BUNDLED_MARKETPLACE_NAME: &str = "openai-bundled";
const REQUIRED_PLUGINS: &[&str] = &[
    "browser@openai-bundled",
    "chrome@openai-bundled",
    "unified-computer-use@openai-bundled",
];

#[derive(Debug, Clone)]
pub struct DesktopRuntime {
    install_root: PathBuf,
    bundled_marketplace: PathBuf,
    node: PathBuf,
    node_repl: PathBuf,
    node_modules: PathBuf,
    cua_repl: PathBuf,
    sky_service: PathBuf,
    app_version: String,
}

impl DesktopRuntime {
    pub fn install_root(&self) -> &Path {
        &self.install_root
    }

    pub fn app_version(&self) -> &str {
        &self.app_version
    }

    pub async fn bootstrap_native_plugins(
        &self,
        codex_home: &Path,
        codex_bin: &Path,
    ) -> Result<()> {
        let managed_root = codex_home
            .join(".tmp")
            .join("bundled-marketplaces")
            .join(BUNDLED_MARKETPLACE_NAME);
        materialize_bundled_marketplace(&self.bundled_marketplace, &managed_root)?;

        run_codex_mutation(
            codex_bin,
            codex_home,
            [
                OsStr::new("plugin"),
                OsStr::new("marketplace"),
                OsStr::new("add"),
                managed_root.as_os_str(),
                OsStr::new("--json"),
            ],
            "register bundled OpenAI marketplace",
        )
        .await?;

        for plugin in REQUIRED_PLUGINS {
            run_codex_mutation(
                codex_bin,
                codex_home,
                [
                    OsStr::new("plugin"),
                    OsStr::new("add"),
                    OsStr::new(plugin),
                    OsStr::new("--json"),
                ],
                &format!("install {plugin}"),
            )
            .await?;
        }

        Ok(())
    }

    pub fn apply_cua_config(
        &self,
        codex: &mut Command,
        codex_home: &Path,
        codex_bin: &Path,
        auth_shim_bin: &Path,
        auth_source_home: &Path,
    ) {
        let trusted_paths = std::env::join_paths([codex_home, self.node_modules.as_path()])
            .expect("join trusted CUA paths")
            .to_string_lossy()
            .into_owned();
        let env = [
            (
                "NODE_REPL_NATIVE_PIPE_CONNECT_TIMEOUT_MS",
                "1000".to_string(),
            ),
            (
                "NODE_REPL_NODE_MODULE_DIRS",
                self.node_modules.to_string_lossy().into_owned(),
            ),
            (
                "NODE_REPL_NODE_PATH",
                self.node.to_string_lossy().into_owned(),
            ),
            (
                "NODE_REPL_UNTRUSTED_ENV_ALLOWLIST",
                "FKN_CODEX_REAL_BIN,FKN_CODEX_AUTH_SOURCE_HOME".to_string(),
            ),
            ("NODE_REPL_TRUSTED_CODE_PATHS", trusted_paths),
            ("CODEX_HOME", codex_home.to_string_lossy().into_owned()),
            ("BROWSER_USE_AVAILABLE_BACKENDS", "chrome".to_string()),
            ("BROWSER_USE_TINYSKY_ENABLED", "1".to_string()),
            (
                "NODE_REPL_INSTRUCTIONS_USE_CASE_BROWSER",
                "Control browser surfaces through Computer Use.".to_string(),
            ),
            (
                "NODE_REPL_INSTRUCTIONS_USE_CASE_CHROME",
                "Control Chrome through the ChatGPT Chrome extension backend.".to_string(),
            ),
            (
                "NODE_REPL_INSTRUCTIONS_USE_CASE_COMPUTER_USE",
                "Control native desktop apps through Computer Use.".to_string(),
            ),
            ("BROWSER_USE_CODEX_APP_BUILD_FLAVOR", "prod".to_string()),
            ("BROWSER_USE_CODEX_APP_VERSION", self.app_version.clone()),
            (
                "NODE_REPL_TRUSTED_SERVICES",
                r#"{"browser":"@oai/browser-desktop/service","sky":"@oai/sky/service"}"#
                    .to_string(),
            ),
            (
                "SKY_CUA_SERVICE_PATH",
                self.sky_service.to_string_lossy().into_owned(),
            ),
            (
                "CODEX_CLI_PATH",
                auth_shim_bin.to_string_lossy().into_owned(),
            ),
            (
                "FKN_CODEX_REAL_BIN",
                codex_bin.to_string_lossy().into_owned(),
            ),
            (
                "FKN_CODEX_AUTH_SOURCE_HOME",
                auth_source_home.to_string_lossy().into_owned(),
            ),
            (
                "CUA_REPL_NODE_REPL_PATH",
                self.node_repl.to_string_lossy().into_owned(),
            ),
            ("CUA_REPL_ENABLED_SURFACES", "browser,computer".to_string()),
        ];

        push_config(
            codex,
            "mcp_servers.cua_repl.command",
            &toml_string(&self.node),
        );
        push_config(
            codex,
            "mcp_servers.cua_repl.args",
            &format!("[{}]", toml_string(&self.cua_repl)),
        );
        push_config(codex, "mcp_servers.cua_repl.enabled", "true");
        push_config(
            codex,
            "mcp_servers.cua_repl.default_tools_approval_mode",
            "\"approve\"",
        );
        push_config(codex, "mcp_servers.cua_repl.startup_timeout_sec", "120");
        for (key, value) in env {
            push_config(
                codex,
                &format!("mcp_servers.cua_repl.env.{key}"),
                &toml_string(value),
            );
        }
    }
}

pub struct DesktopRuntimeLocator;

impl DesktopRuntimeLocator {
    pub fn locate(explicit_install: Option<&Path>) -> Result<DesktopRuntime> {
        if let Some(path) = explicit_install {
            return locate_explicit(path);
        }
        locate_default()
    }
}

#[cfg(target_os = "macos")]
fn locate_default() -> Result<DesktopRuntime> {
    let mut candidates = vec![PathBuf::from("/Applications/ChatGPT.app")];
    if let Some(home) = std::env::var_os("HOME").filter(|value| !value.is_empty()) {
        candidates.push(PathBuf::from(home).join("Applications/ChatGPT.app"));
    }

    for candidate in candidates {
        if candidate.is_dir() {
            return discover_macos(&candidate);
        }
    }

    anyhow::bail!(
        "ChatGPT Desktop was not found. Install the official ChatGPT Desktop app, pass --chatgpt-app /path/to/ChatGPT.app, or use --disable-cua."
    )
}

#[cfg(target_os = "macos")]
fn locate_explicit(path: &Path) -> Result<DesktopRuntime> {
    discover_macos(path)
}

#[cfg(target_os = "macos")]
fn discover_macos(chatgpt_app: &Path) -> Result<DesktopRuntime> {
    anyhow::ensure!(
        chatgpt_app.is_dir(),
        "ChatGPT Desktop was not found at {}",
        chatgpt_app.display()
    );
    let install_root = std::fs::canonicalize(chatgpt_app)
        .with_context(|| format!("resolve ChatGPT Desktop {}", chatgpt_app.display()))?;
    let resources = install_root.join("Contents/Resources");
    let bundled_marketplace = resources.join("plugins/openai-bundled");
    let node = resources.join("cua_node/bin/node");
    let node_repl = resources.join("cua_node/bin/node_repl");
    let node_modules = resources.join("cua_node/lib/node_modules");
    let cua_repl = node_modules.join("@oai/cua-repl/bin/cua-repl.mjs");
    let sky_service = node_modules.join("@oai/sky/Codex Computer Use.app");
    let plugin_manifest = resources
        .join("plugins/openai-bundled/plugins/unified-computer-use/.codex-plugin/plugin.json");

    validate_required_resources([
        &bundled_marketplace,
        &node,
        &node_repl,
        &node_modules,
        &cua_repl,
        &sky_service,
        &plugin_manifest,
    ])?;

    let app_version = read_plugin_version(&plugin_manifest)?;
    Ok(DesktopRuntime {
        install_root,
        bundled_marketplace,
        node,
        node_repl,
        node_modules,
        cua_repl,
        sky_service,
        app_version,
    })
}

#[cfg(target_os = "windows")]
fn locate_default() -> Result<DesktopRuntime> {
    anyhow::bail!(
        "automatic ChatGPT Desktop runtime discovery is not implemented on Windows yet; pass --chatgpt-app to a supported extracted/runtime root once the Windows layout backend is added, or use --disable-cua"
    )
}

#[cfg(target_os = "windows")]
fn locate_explicit(path: &Path) -> Result<DesktopRuntime> {
    anyhow::bail!(
        "Windows ChatGPT Desktop runtime layout support is not implemented yet (requested root: {})",
        path.display()
    )
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn locate_default() -> Result<DesktopRuntime> {
    anyhow::bail!("ChatGPT Desktop Browser/Computer Use runtime is unsupported on this platform")
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn locate_explicit(path: &Path) -> Result<DesktopRuntime> {
    anyhow::bail!(
        "ChatGPT Desktop Browser/Computer Use runtime is unsupported on this platform (requested root: {})",
        path.display()
    )
}

#[cfg(target_os = "macos")]
fn validate_required_resources<'a>(resources: impl IntoIterator<Item = &'a PathBuf>) -> Result<()> {
    for required in resources {
        anyhow::ensure!(
            required.exists(),
            "required ChatGPT Desktop runtime resource is missing: {}",
            required.display()
        );
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn read_plugin_version(plugin_manifest: &Path) -> Result<String> {
    let manifest: Value = serde_json::from_slice(
        &std::fs::read(plugin_manifest)
            .with_context(|| format!("read {}", plugin_manifest.display()))?,
    )
    .with_context(|| format!("parse {}", plugin_manifest.display()))?;
    manifest
        .get("version")
        .and_then(Value::as_str)
        .map(str::to_string)
        .context("unified-computer-use plugin manifest has no version")
}

fn materialize_bundled_marketplace(source: &Path, managed_root: &Path) -> Result<()> {
    std::fs::create_dir_all(managed_root)
        .with_context(|| format!("create managed marketplace {}", managed_root.display()))?;

    for relative in [".agents", "plugins", ".bundle-id"] {
        let source_path = source.join(relative);
        if !source_path.exists() {
            continue;
        }
        let destination = managed_root.join(relative);
        ensure_managed_link(&source_path, &destination)?;
    }
    Ok(())
}

#[cfg(unix)]
fn ensure_managed_link(source: &Path, destination: &Path) -> Result<()> {
    use std::os::unix::fs::symlink;

    if let Ok(metadata) = std::fs::symlink_metadata(destination) {
        if metadata.file_type().is_symlink() {
            let target = std::fs::read_link(destination)
                .with_context(|| format!("read symlink {}", destination.display()))?;
            if target == source {
                return Ok(());
            }
            std::fs::remove_file(destination)
                .with_context(|| format!("replace symlink {}", destination.display()))?;
        } else {
            anyhow::bail!(
                "managed marketplace path is occupied by a non-symlink: {}",
                destination.display()
            );
        }
    }

    symlink(source, destination).with_context(|| {
        format!(
            "link ChatGPT marketplace resource {} -> {}",
            destination.display(),
            source.display()
        )
    })
}

#[cfg(windows)]
fn ensure_managed_link(_source: &Path, destination: &Path) -> Result<()> {
    anyhow::bail!(
        "Windows managed marketplace materialization backend is not implemented yet: {}",
        destination.display()
    )
}

#[cfg(not(any(unix, windows)))]
fn ensure_managed_link(_source: &Path, destination: &Path) -> Result<()> {
    anyhow::bail!(
        "managed marketplace linking is unsupported on this platform: {}",
        destination.display()
    )
}

async fn run_codex_mutation<I, S>(
    codex_bin: &Path,
    codex_home: &Path,
    args: I,
    operation: &str,
) -> Result<Value>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = Command::new(codex_bin)
        .args(args)
        .env("CODEX_HOME", codex_home)
        .stdin(Stdio::null())
        .output()
        .await
        .with_context(|| format!("run Codex mutation: {operation}"))?;

    anyhow::ensure!(
        output.status.success(),
        "Codex mutation failed ({operation}): {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );

    serde_json::from_slice(&output.stdout).with_context(|| {
        format!(
            "parse Codex mutation output ({operation}): {}",
            String::from_utf8_lossy(&output.stdout).trim()
        )
    })
}

fn toml_string(value: impl AsRef<OsStr>) -> String {
    serde_json::to_string(&value.as_ref().to_string_lossy()).expect("serialize TOML string")
}

fn push_config(codex: &mut Command, key: &str, value: &str) {
    codex.arg("-c").arg(format!("{key}={value}"));
}
