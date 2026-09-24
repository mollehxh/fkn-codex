use anyhow::Context as _;
use anyhow::Result;
use serde::Deserialize;
use std::collections::VecDeque;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use super::settings::{AppPaths, Settings};

const MAX_LOG_LINES: usize = 300;
const TUNNEL_READY_TIMEOUT: Duration = Duration::from_secs(20);
const RESTART_BACKOFF: Duration = Duration::from_secs(2);

#[derive(Clone, Debug)]
pub struct Binaries {
    pub bridge: Option<PathBuf>,
    pub codex: Option<PathBuf>,
    pub auth_shim: Option<PathBuf>,
    pub tunnel: Option<PathBuf>,
}

impl Binaries {
    pub fn discover() -> Self {
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|path| path.parent().map(Path::to_path_buf));
        let cwd = std::env::current_dir().ok();
        let version_two = std::env::current_exe()
            .ok()
            .and_then(|path| path.file_stem().map(|name| name == "fkn-codex-2"))
            .unwrap_or(false);
        let bridge_name = if version_two {
            "fkn-codex-bridge-2"
        } else {
            "fkn-codex-bridge"
        };
        let bridge_env = if version_two {
            "FKN_CODEX_BRIDGE_2_BIN"
        } else {
            "FKN_CODEX_BRIDGE_BIN"
        };
        Self {
            bridge: resolve_binary(
                bridge_env,
                executable_name(bridge_name),
                exe_dir.as_deref(),
                cwd.as_deref()
                    .map(|root| root.join(format!("fkn-bridge/target/debug/{bridge_name}"))),
            ),
            codex: resolve_binary(
                "FKN_CODEX_BIN",
                executable_name("codex"),
                exe_dir.as_deref(),
                cwd.as_deref()
                    .map(|root| root.join("codex-rs/target/debug/codex")),
            ),
            auth_shim: resolve_binary(
                "FKN_CODEX_AUTH_SHIM_BIN",
                executable_name("fkn-codex-auth-shim"),
                exe_dir.as_deref(),
                cwd.as_deref()
                    .map(|root| root.join("fkn-bridge/target/debug/fkn-codex-auth-shim")),
            ),
            tunnel: resolve_binary(
                "FKN_TUNNEL_CLIENT_BIN",
                executable_name("tunnel-client"),
                exe_dir.as_deref(),
                home_dir().map(|home| {
                    home.join(".local/bin")
                        .join(executable_name("tunnel-client"))
                }),
            ),
        }
    }
}

fn executable_name(base: &str) -> &'static str {
    match base {
        "fkn-codex-bridge" if cfg!(windows) => "fkn-codex-bridge.exe",
        "fkn-codex-bridge" => "fkn-codex-bridge",
        "fkn-codex-bridge-2" if cfg!(windows) => "fkn-codex-bridge-2.exe",
        "fkn-codex-bridge-2" => "fkn-codex-bridge-2",
        "codex" if cfg!(windows) => "codex.exe",
        "codex" => "codex",
        "fkn-codex-auth-shim" if cfg!(windows) => "fkn-codex-auth-shim.exe",
        "fkn-codex-auth-shim" => "fkn-codex-auth-shim",
        "tunnel-client" if cfg!(windows) => "tunnel-client.exe",
        "tunnel-client" => "tunnel-client",
        _ => unreachable!(),
    }
}

fn resolve_binary(
    env_name: &str,
    name: &str,
    sibling_dir: Option<&Path>,
    extra: Option<PathBuf>,
) -> Option<PathBuf> {
    if let Some(value) = std::env::var_os(env_name).filter(|value| !value.is_empty()) {
        let path = PathBuf::from(value);
        if path.is_file() {
            return Some(path);
        }
    }
    if let Some(dir) = sibling_dir {
        let path = dir.join(name);
        if path.is_file() {
            return Some(path);
        }
    }
    if let Some(path) = extra
        && path.is_file()
    {
        return Some(path);
    }
    find_in_path(name)
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file())
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

#[derive(Debug, Deserialize)]
struct BridgeReady {
    mcp_url: String,
}

pub struct RuntimeController {
    bridge: Option<Child>,
    tunnel: Option<Child>,
    mcp_url: Option<String>,
    health_url: Option<String>,
    state_dir: PathBuf,
    logs: Arc<Mutex<VecDeque<String>>>,
    keep_bridge_running: bool,
    keep_tunnel_running: bool,
    next_restart_at: Option<Instant>,
}

impl RuntimeController {
    pub fn new(state_dir: PathBuf) -> Result<Self> {
        fs::create_dir_all(&state_dir)?;
        Ok(Self {
            bridge: None,
            tunnel: None,
            mcp_url: None,
            health_url: None,
            state_dir,
            logs: Arc::new(Mutex::new(VecDeque::new())),
            keep_bridge_running: false,
            keep_tunnel_running: false,
            next_restart_at: None,
        })
    }

    pub fn bridge_running(&mut self) -> bool {
        child_running(&mut self.bridge)
    }

    pub fn tunnel_running(&mut self) -> bool {
        child_running(&mut self.tunnel)
    }

    pub fn mcp_url(&self) -> Option<&str> {
        self.mcp_url.as_deref()
    }

    pub fn health_url(&self) -> Option<&str> {
        self.health_url.as_deref()
    }

    pub fn logs(&self) -> Vec<String> {
        self.logs
            .lock()
            .map(|logs| logs.iter().cloned().collect())
            .unwrap_or_default()
    }

    pub fn refresh(&mut self) {
        let bridge_alive = self.bridge_running();
        let tunnel_alive = self.tunnel_running();
        if !bridge_alive {
            self.mcp_url = None;
        }
        if tunnel_alive {
            let health_file = self.state_dir.join("tunnel-health.url");
            if let Ok(value) = fs::read_to_string(health_file) {
                let value = value.trim();
                if !value.is_empty() {
                    self.health_url = Some(value.to_string());
                }
            }
        } else {
            self.health_url = None;
        }
    }

    pub fn start_all(
        &mut self,
        workspace: &Path,
        paths: &AppPaths,
        settings: &Settings,
        binaries: &Binaries,
    ) -> Result<()> {
        if !self.bridge_running() {
            self.start_bridge(workspace, paths, settings, binaries)?;
        }
        if !self.tunnel_running() {
            self.start_tunnel(paths, settings, binaries)?;
        }
        self.keep_bridge_running = true;
        self.keep_tunnel_running = true;
        self.next_restart_at = None;
        Ok(())
    }

    pub fn resume_tunnel(
        &mut self,
        paths: &AppPaths,
        settings: &Settings,
        binaries: &Binaries,
    ) -> Result<()> {
        anyhow::ensure!(self.bridge_running(), "local bridge is not running");
        if !self.tunnel_running() {
            self.start_tunnel(paths, settings, binaries)?;
        }
        self.keep_bridge_running = true;
        self.keep_tunnel_running = true;
        self.next_restart_at = None;
        Ok(())
    }

    pub fn maintain(
        &mut self,
        workspace: &Path,
        paths: &AppPaths,
        settings: &Settings,
        binaries: &Binaries,
    ) {
        if !self.keep_bridge_running
            || self
                .next_restart_at
                .is_some_and(|deadline| Instant::now() < deadline)
        {
            return;
        }

        let bridge_alive = self.bridge_running();
        let tunnel_alive = self.tunnel_running();
        let result = if !bridge_alive {
            if let Some(mut tunnel) = self.tunnel.take() {
                terminate_process_tree(&mut tunnel);
            }
            self.health_url = None;
            let _ = fs::remove_file(self.state_dir.join("tunnel-health.url"));
            self.push_log("local bridge stopped; restarting runtime");
            self.start_bridge(workspace, paths, settings, binaries)
                .and_then(|()| {
                    if self.keep_tunnel_running {
                        self.start_tunnel(paths, settings, binaries)
                    } else {
                        Ok(())
                    }
                })
        } else if self.keep_tunnel_running && !tunnel_alive {
            self.push_log("OpenAI tunnel stopped; reconnecting");
            self.start_tunnel(paths, settings, binaries)
        } else {
            return;
        };

        if let Err(error) = result {
            self.push_log(format!("automatic runtime recovery failed: {error:#}"));
            self.next_restart_at = Some(Instant::now() + RESTART_BACKOFF);
        } else {
            self.next_restart_at = None;
            self.push_log("automatic runtime recovery completed");
        }
    }

    pub fn pause(&mut self) {
        self.keep_tunnel_running = false;
        if let Some(mut tunnel) = self.tunnel.take() {
            terminate_process_tree(&mut tunnel);
        }
        self.health_url = None;
        let _ = fs::remove_file(self.state_dir.join("tunnel-health.url"));
        self.push_log("tunnel paused; local bridge remains alive");
    }

    pub fn stop(&mut self) {
        self.keep_bridge_running = false;
        self.keep_tunnel_running = false;
        self.next_restart_at = None;
        self.pause();
        if let Some(mut bridge) = self.bridge.take() {
            terminate_process_tree(&mut bridge);
        }
        self.mcp_url = None;
        let _ = fs::remove_file(self.state_dir.join("bridge-ready.json"));
        self.push_log("local runtime stopped");
    }

    fn start_bridge(
        &mut self,
        workspace: &Path,
        paths: &AppPaths,
        settings: &Settings,
        binaries: &Binaries,
    ) -> Result<()> {
        let bridge_bin = binaries
            .bridge
            .as_deref()
            .context("fkn-codex-bridge binary not found")?;
        let codex_bin = binaries
            .codex
            .as_deref()
            .context("bundled Codex binary not found")?;
        let auth_shim = if settings.computer_use {
            Some(
                binaries
                    .auth_shim
                    .as_deref()
                    .context("FKN Browser auth shim binary not found")?,
            )
        } else {
            None
        };
        let ready_file = self.state_dir.join("bridge-ready.json");
        let _ = fs::remove_file(&ready_file);

        let mut command = Command::new(bridge_bin);
        command
            .arg("--workspace")
            .arg(workspace)
            .arg("--codex-bin")
            .arg(codex_bin)
            .arg("--codex-home")
            .arg(&paths.codex_home)
            .arg("--listen")
            .arg("127.0.0.1:0")
            .arg("--sandbox")
            .arg(settings.permission.cli_value())
            .arg("--ready-file")
            .arg(&ready_file)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(auth_shim) = auth_shim {
            command.arg("--auth-shim-bin").arg(auth_shim);
        }
        if !settings.computer_use {
            command.arg("--disable-cua");
        }
        configure_process_group(&mut command);
        let mut child = command.spawn().context("start fkn-codex-bridge")?;
        capture_output(&mut child, Arc::clone(&self.logs), "bridge");

        let deadline = Instant::now() + Duration::from_secs(30);
        let startup = loop {
            if let Ok(text) = fs::read_to_string(&ready_file)
                && let Ok(ready) = serde_json::from_str::<BridgeReady>(&text)
            {
                break Ok(ready.mcp_url);
            }
            match child.try_wait().context("read bridge status") {
                Ok(Some(status)) => {
                    break Err(anyhow::anyhow!(
                        "local bridge exited during startup with {status}"
                    ));
                }
                Ok(None) => {}
                Err(error) => break Err(error),
            }
            if Instant::now() >= deadline {
                break Err(anyhow::anyhow!("timed out waiting for local bridge"));
            }
            thread::sleep(Duration::from_millis(50));
        };
        match startup {
            Ok(mcp_url) => {
                self.mcp_url = Some(mcp_url);
                self.bridge = Some(child);
                self.push_log("local bridge ready");
                Ok(())
            }
            Err(error) => {
                terminate_process_tree(&mut child);
                let _ = fs::remove_file(&ready_file);
                Err(error)
            }
        }
    }

    fn start_tunnel(
        &mut self,
        paths: &AppPaths,
        settings: &Settings,
        binaries: &Binaries,
    ) -> Result<()> {
        let tunnel_bin = binaries
            .tunnel
            .as_deref()
            .context("OpenAI tunnel-client binary not found")?;
        let mcp_url = self
            .mcp_url
            .as_deref()
            .context("local MCP URL is unavailable")?;
        let health_file = self.state_dir.join("tunnel-health.url");
        let _ = fs::remove_file(&health_file);
        let api_key_ref = format!("file:{}", paths.api_key_file.display());
        let mcp_binding = format!("url={mcp_url},channel=main");

        let mut command = Command::new(tunnel_bin);
        command
            .arg("run")
            .arg("--control-plane.tunnel-id")
            .arg(settings.tunnel_id.trim())
            .arg("--control-plane.api-key")
            .arg(api_key_ref)
            .arg("--mcp.server-url")
            .arg(mcp_binding)
            .arg("--mcp.startup-wait-timeout")
            .arg("10s")
            .arg("--health.listen-addr")
            .arg("127.0.0.1:0")
            .arg("--health.url-file")
            .arg(&health_file)
            .arg("--log.format")
            .arg("struct-text")
            .arg("--log.level")
            .arg("info")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(proxy) = control_plane_proxy() {
            command.arg("--control-plane.http-proxy").arg(proxy);
            self.push_log("using configured proxy for OpenAI control-plane");
        }
        configure_process_group(&mut command);
        let mut child = command.spawn().context("start OpenAI tunnel-client")?;
        capture_output(&mut child, Arc::clone(&self.logs), "tunnel");
        match wait_for_tunnel_ready(&mut child, &health_file) {
            Ok(health_url) => {
                self.health_url = Some(health_url);
                self.tunnel = Some(child);
                self.push_log("OpenAI tunnel ready");
                Ok(())
            }
            Err(error) => {
                terminate_process_tree(&mut child);
                let _ = fs::remove_file(&health_file);
                Err(error)
            }
        }
    }

    fn push_log(&self, line: impl Into<String>) {
        push_log(&self.logs, line.into());
    }
}

impl Drop for RuntimeController {
    fn drop(&mut self) {
        self.stop();
    }
}

fn child_running(child: &mut Option<Child>) -> bool {
    let Some(process) = child.as_mut() else {
        return false;
    };
    match process.try_wait() {
        Ok(None) => true,
        Ok(Some(_)) | Err(_) => {
            *child = None;
            false
        }
    }
}

fn capture_output(child: &mut Child, logs: Arc<Mutex<VecDeque<String>>>, prefix: &'static str) {
    if let Some(stdout) = child.stdout.take() {
        let logs = Arc::clone(&logs);
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                push_log(&logs, format!("[{prefix}] {line}"));
            }
        });
    }
    if let Some(stderr) = child.stderr.take() {
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                push_log(&logs, format!("[{prefix}] {line}"));
            }
        });
    }
}

fn push_log(logs: &Arc<Mutex<VecDeque<String>>>, line: String) {
    if let Ok(mut logs) = logs.lock() {
        if logs.len() >= MAX_LOG_LINES {
            logs.pop_front();
        }
        logs.push_back(line);
    }
}

fn control_plane_proxy() -> Option<String> {
    [
        "FKN_TUNNEL_CONTROL_PLANE_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "HTTP_PROXY",
    ]
    .into_iter()
    .find_map(|name| std::env::var(name).ok().and_then(normalize_proxy))
    .or_else(windows_system_proxy)
}

fn normalize_proxy(value: String) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let selected = value
        .split(';')
        .find_map(|entry| {
            let (kind, proxy) = entry.split_once('=')?;
            kind.trim()
                .eq_ignore_ascii_case("https")
                .then_some(proxy.trim())
        })
        .or_else(|| {
            value
                .split(';')
                .find_map(|entry| entry.split_once('=').map(|(_, proxy)| proxy.trim()))
        })
        .unwrap_or(value);
    if selected.is_empty() {
        None
    } else if selected.contains("://") {
        Some(selected.to_string())
    } else {
        Some(format!("http://{selected}"))
    }
}

#[cfg(windows)]
fn windows_system_proxy() -> Option<String> {
    use std::os::windows::process::CommandExt as _;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const INTERNET_SETTINGS: &str =
        r"HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings";
    let query = |value: &str| {
        Command::new("reg.exe")
            .args(["query", INTERNET_SETTINGS, "/v", value])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| String::from_utf8(output.stdout).ok())
    };
    let enabled = query("ProxyEnable")?;
    if !enabled
        .split_whitespace()
        .any(|token| token == "0x1" || token == "1")
    {
        return None;
    }
    let server = query("ProxyServer")?;
    let value = server
        .lines()
        .find(|line| line.contains("ProxyServer"))?
        .split_whitespace()
        .last()?
        .to_string();
    normalize_proxy(value)
}

#[cfg(not(windows))]
fn windows_system_proxy() -> Option<String> {
    None
}

#[cfg(unix)]
fn configure_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt as _;
    command.process_group(0);
}

#[cfg(not(unix))]
fn configure_process_group(_command: &mut Command) {}

#[cfg(unix)]
fn terminate_process_tree(child: &mut Child) {
    let pid = child.id() as i32;
    unsafe {
        libc::kill(-pid, libc::SIGTERM);
    }
    for _ in 0..20 {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
    }
    let _ = child.wait();
}

#[cfg(windows)]
fn terminate_process_tree(child: &mut Child) {
    use std::os::windows::process::CommandExt as _;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let pid = child.id().to_string();
    let terminated = Command::new("taskkill.exe")
        .args(["/PID", &pid, "/T", "/F"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .is_ok_and(|status| status.success());
    if !terminated {
        let _ = child.kill();
    }
    let _ = child.wait();
}

fn wait_for_tunnel_ready(child: &mut Child, health_file: &Path) -> Result<String> {
    let deadline = Instant::now() + TUNNEL_READY_TIMEOUT;
    loop {
        if let Some(status) = child.try_wait().context("read tunnel status")? {
            anyhow::bail!(
                "OpenAI tunnel-client exited during startup with {status}. Use a runtime API key from Organization > API keys and verify Tunnels Read + Use"
            );
        }
        if let Ok(health_url) = fs::read_to_string(health_file) {
            let health_url = health_url.trim();
            if !health_url.is_empty() && tunnel_is_ready(health_url) {
                return Ok(health_url.to_string());
            }
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "OpenAI tunnel did not become ready. Use a runtime API key from Organization > API keys and verify Tunnels Read + Use and the ChatGPT workspace association"
        );
        thread::sleep(Duration::from_millis(100));
    }
}

fn tunnel_is_ready(health_url: &str) -> bool {
    let Some(authority) = health_url
        .strip_prefix("http://")
        .map(|value| value.trim_end_matches('/'))
    else {
        return false;
    };
    let Ok(address) = authority.parse::<SocketAddr>() else {
        return false;
    };
    let Ok(mut stream) = TcpStream::connect_timeout(&address, Duration::from_millis(250)) else {
        return false;
    };
    if stream
        .set_read_timeout(Some(Duration::from_millis(250)))
        .is_err()
        || stream
            .set_write_timeout(Some(Duration::from_millis(250)))
            .is_err()
    {
        return false;
    }
    let request = format!("GET /readyz HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n");
    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }
    let mut status_line = String::new();
    let Ok(read) = BufReader::new(stream).read_line(&mut status_line) else {
        return false;
    };
    read > 0
        && (status_line.starts_with("HTTP/1.1 200 ") || status_line.starts_with("HTTP/1.0 200 "))
}

#[cfg(not(any(unix, windows)))]
fn terminate_process_tree(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(test)]
#[path = "runtime_tests.rs"]
mod tests;
