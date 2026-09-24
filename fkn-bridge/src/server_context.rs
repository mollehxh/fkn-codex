use std::path::Path;
use std::path::PathBuf;

pub(crate) const MAX_SERVER_INSTRUCTIONS_BYTES: usize = 1024;
const MAX_WORKSPACE_CONTEXT_BYTES: usize = 512;
const MAX_SHELL_CONTEXT_BYTES: usize = 64;

/// Filesystem access granted to the hidden Codex runtime behind the MCP server.
#[derive(Clone, Copy, Debug)]
pub enum CodexAccessMode {
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
}

impl CodexAccessMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
            Self::DangerFullAccess => "danger-full-access",
        }
    }
}

pub(crate) enum ComputerUseStatus {
    Enabled,
    Disabled,
}

impl ComputerUseStatus {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Enabled => "enabled",
            Self::Disabled => "disabled",
        }
    }
}

pub(crate) fn render(
    workspace: &Path,
    access_mode: CodexAccessMode,
    computer_use: ComputerUseStatus,
) -> Option<String> {
    let workspace =
        bounded_context_field(&workspace.to_string_lossy(), MAX_WORKSPACE_CONTEXT_BYTES);
    let workspace = serde_json::to_string(&workspace).ok()?;
    let shell = if cfg!(windows) {
        "powershell".to_string()
    } else {
        let configured_shell = std::env::var_os("SHELL").map(PathBuf::from);
        configured_shell
            .as_deref()
            .and_then(Path::file_name)
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "sh".to_string())
    };
    let computer_use = computer_use.as_str();
    let shell =
        serde_json::to_string(&bounded_context_field(&shell, MAX_SHELL_CONTEXT_BYTES)).ok()?;
    let instructions = format!(
        "Local Codex controller. Tools run on the user's local machine. Environment: os={}/{}; shell={shell}; access={}; computer_use={computer_use}; cwd={workspace}. Treat cwd as the default workspace and use absolute paths when operating elsewhere.",
        std::env::consts::OS,
        std::env::consts::ARCH,
        access_mode.as_str(),
    );
    Some(truncate_utf8(instructions, MAX_SERVER_INSTRUCTIONS_BYTES))
}

fn bounded_context_field(value: &str, max_bytes: usize) -> String {
    truncate_utf8(value.to_string(), max_bytes)
}

fn truncate_utf8(mut value: String, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value;
    }
    let mut boundary = max_bytes.saturating_sub(3);
    while boundary > 0 && !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value.truncate(boundary);
    value.push_str("...");
    value
}
