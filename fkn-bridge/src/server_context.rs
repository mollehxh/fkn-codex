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

pub(crate) fn render(workspace: &Path, access_mode: CodexAccessMode) -> Option<String> {
    let workspace =
        bounded_context_field(&workspace.to_string_lossy(), MAX_WORKSPACE_CONTEXT_BYTES);
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
    let shell = bounded_context_field(&shell, MAX_SHELL_CONTEXT_BYTES);
    let os = match std::env::consts::OS {
        "macos" => "macOS",
        "windows" => "Windows",
        "linux" => "Linux",
        other => other,
    };
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "x64",
        other => other,
    };
    let instructions = format!(
        "At the start of each conversation, call `codex_skills_list` once to get the skill list. Call it again only if the available skills may have changed. If Browser Use, Computer Use, or any CUA tool returns `tool_unavailable`, do not perform the same action via shell, scripts, direct HTTP, browser automation, or another workaround unless the user explicitly asks. Report the unavailable tool and `requiredAction`, if provided. Environment: {os}/{arch}, {shell}, {}, cwd={workspace}.",
        access_mode.as_str(),
    );
    Some(truncate_utf8(instructions, MAX_SERVER_INSTRUCTIONS_BYTES))
}

fn bounded_context_field(value: &str, max_bytes: usize) -> String {
    truncate_utf8(value.to_string(), max_bytes)
}

pub(crate) fn truncate_utf8(mut value: String, max_bytes: usize) -> String {
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
