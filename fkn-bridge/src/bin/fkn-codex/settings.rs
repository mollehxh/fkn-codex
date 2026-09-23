use anyhow::Context as _;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionMode {
    ReadOnly,
    #[default]
    WorkspaceWrite,
    DangerFullAccess,
}

impl PermissionMode {
    pub const ALL: [Self; 3] = [Self::WorkspaceWrite, Self::ReadOnly, Self::DangerFullAccess];

    pub fn label(self) -> &'static str {
        match self {
            Self::ReadOnly => "Read only",
            Self::WorkspaceWrite => "Workspace write",
            Self::DangerFullAccess => "Full access",
        }
    }

    pub fn cli_value(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
            Self::DangerFullAccess => "danger-full-access",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Settings {
    #[serde(default)]
    pub tunnel_id: String,
    #[serde(default)]
    pub permission: PermissionMode,
    #[serde(default = "default_true")]
    pub computer_use: bool,
}

fn default_true() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            tunnel_id: String::new(),
            permission: PermissionMode::WorkspaceWrite,
            computer_use: true,
        }
    }
}

#[derive(Clone, Debug)]
pub struct AppPaths {
    pub config_dir: PathBuf,
    pub state_dir: PathBuf,
    pub codex_home: PathBuf,
    pub settings_file: PathBuf,
    pub api_key_file: PathBuf,
}

impl AppPaths {
    pub fn discover(override_root: Option<PathBuf>) -> Result<Self> {
        let config_dir = match override_root {
            Some(root) => root,
            None => default_config_dir()?,
        };
        let state_dir = config_dir.join("state");
        let codex_home = config_dir.join("codex-home");
        fs::create_dir_all(&state_dir)
            .with_context(|| format!("create {}", state_dir.display()))?;
        fs::create_dir_all(&codex_home)
            .with_context(|| format!("create {}", codex_home.display()))?;
        Ok(Self {
            settings_file: config_dir.join("config.json"),
            api_key_file: config_dir.join("runtime-api-key"),
            config_dir,
            state_dir,
            codex_home,
        })
    }

    pub fn load_settings(&self) -> Result<Settings> {
        match fs::read_to_string(&self.settings_file) {
            Ok(text) => serde_json::from_str(&text)
                .with_context(|| format!("parse {}", self.settings_file.display())),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Settings::default()),
            Err(error) => {
                Err(error).with_context(|| format!("read {}", self.settings_file.display()))
            }
        }
    }

    pub fn save_settings(&self, settings: &Settings) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(settings)?;
        atomic_write(&self.settings_file, &bytes)
    }

    pub fn save_api_key(&self, api_key: &str) -> Result<()> {
        atomic_write(&self.api_key_file, api_key.trim().as_bytes())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&self.api_key_file, fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }

    pub fn has_api_key(&self) -> bool {
        fs::metadata(&self.api_key_file)
            .map(|metadata| metadata.is_file() && metadata.len() > 0)
            .unwrap_or(false)
    }
}

pub fn valid_tunnel_id(value: &str) -> bool {
    value
        .trim()
        .strip_prefix("tunnel_")
        .is_some_and(|suffix| !suffix.is_empty())
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("path has no parent")?;
    fs::create_dir_all(parent)?;
    let temp = path.with_extension("tmp");
    fs::write(&temp, bytes).with_context(|| format!("write {}", temp.display()))?;
    fs::rename(&temp, path).with_context(|| format!("replace {}", path.display()))?;
    Ok(())
}

fn default_config_dir() -> Result<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var_os("HOME").context("HOME is not set")?;
        Ok(PathBuf::from(home)
            .join("Library")
            .join("Application Support")
            .join("FKN Codex"))
    }
    #[cfg(target_os = "windows")]
    {
        let appdata = std::env::var_os("APPDATA").context("APPDATA is not set")?;
        Ok(PathBuf::from(appdata).join("FKN Codex"))
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
            return Ok(PathBuf::from(xdg).join("fkn-codex"));
        }
        let home = std::env::var_os("HOME").context("HOME is not set")?;
        Ok(PathBuf::from(home).join(".config/fkn-codex"))
    }
}
