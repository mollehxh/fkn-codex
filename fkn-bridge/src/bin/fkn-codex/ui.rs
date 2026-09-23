use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use super::settings::PermissionMode;
use super::{App, View};

const ACCENT: Color = Color::Rgb(217, 119, 87);
const WARNING: Color = Color::Rgb(229, 165, 75);

fn brand() -> Style {
    Style::new().fg(ACCENT)
}

fn muted(text: impl Into<String>) -> Line<'static> {
    Line::from(Span::styled(text.into(), Style::new().dim()))
}

fn strong(text: impl Into<String>) -> Line<'static> {
    Line::from(Span::styled(text.into(), Style::new().bold()))
}

fn pair(label: &str, value: impl Into<String>) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<14}"), Style::new().dim()),
        Span::raw(value.into()),
    ])
}

fn centered(screen: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(screen.width);
    let height = height.min(screen.height);
    Rect::new(
        screen.x + screen.width.saturating_sub(width) / 2,
        screen.y + screen.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

fn choice(selected: bool, label: impl Into<String>, hint: &str) -> Line<'static> {
    let label = label.into();
    Line::from(vec![
        Span::styled(if selected { "› " } else { "  " }, brand().bold()),
        Span::styled(
            format!("{label:<27}"),
            if selected {
                Style::new().bold()
            } else {
                Style::new()
            },
        ),
        Span::styled(
            if selected {
                "Enter".to_string()
            } else {
                hint.to_string()
            },
            if selected {
                brand()
            } else {
                Style::new().dim()
            },
        ),
    ])
}

fn detail_layout(frame: &mut Frame<'_>, title: &str) -> Rect {
    let screen = frame.area();
    let margin = if screen.width >= 64 { 2 } else { 1 };
    let area = Rect::new(
        screen.x + margin,
        screen.y + 1,
        (screen.width - margin * 2).min(100),
        screen.height.saturating_sub(2),
    );
    let sections = Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).split(area);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            title.to_string(),
            Style::new().bold(),
        )))
        .block(
            Block::new()
                .borders(Borders::BOTTOM)
                .border_style(Style::new().dim()),
        ),
        sections[0],
    );
    sections[1]
}

pub fn draw(frame: &mut Frame<'_>, app: &mut App) {
    let screen = frame.area();
    if screen.width < 48 || screen.height < 14 {
        frame.render_widget(
            Paragraph::new(vec![
                strong("FKN Codex"),
                Line::from("Resize to at least 48 × 14."),
                muted("q quit"),
            ]),
            screen,
        );
        return;
    }

    match app.view.clone() {
        View::Home => home(frame, app),
        View::Settings { selected } => settings(frame, app, selected),
        View::Connection {
            field,
            tunnel_id,
            api_key,
        } => connection(frame, app, field, &tunnel_id, &api_key),
        View::Permissions { selected } => permissions(frame, app, selected),
        View::Diagnostics => diagnostics(frame, app),
        View::Help => help(frame),
    }
}

fn home(frame: &mut Frame<'_>, app: &mut App) {
    app.runtime.refresh();
    let screen = frame.area();
    let area = centered(screen, 62, 20);
    let tunnel = app.runtime.tunnel_running();
    let bridge = app.runtime.bridge_running();
    let configured = app.is_configured();
    let primary = if !configured {
        "Set up connection"
    } else if tunnel {
        "Pause"
    } else if bridge {
        "Resume"
    } else {
        "Start"
    };
    let choices = [primary, "Settings", "Diagnostics"];
    app.home_selected = app.home_selected.min(choices.len() - 1);

    let mut lines = vec![
        Line::from(vec![
            Span::styled("█  ", brand()),
            Span::styled("FKN Codex", Style::new().bold()),
        ]),
        muted(app.workspace.display().to_string()),
        Line::from(""),
        pair(
            "Status",
            if tunnel {
                "● tunnel running"
            } else if bridge {
                "● paused"
            } else {
                "○ stopped"
            },
        ),
        pair("Permission", app.settings.permission.label()),
        pair(
            "Computer Use",
            if app.settings.computer_use {
                "enabled"
            } else {
                "disabled"
            },
        ),
        Line::from(""),
    ];

    for (index, label) in choices.iter().enumerate() {
        lines.push(choice(
            app.home_selected == index,
            *label,
            match index {
                1 => "s",
                2 => "d",
                _ => "",
            },
        ));
    }

    lines.push(Line::from(""));
    if !app.status.is_empty() {
        lines.push(Line::from(Span::styled(
            app.status.clone(),
            if app.status.starts_with("Error:") {
                Style::new().red()
            } else {
                Style::new().dim()
            },
        )));
    }
    lines.push(muted("↑↓ select · Enter · ? help · q quit"));
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn settings(frame: &mut Frame<'_>, app: &App, selected: usize) {
    let area = detail_layout(frame, "settings");
    let connection = if app.is_configured() {
        "configured"
    } else {
        "setup required"
    };
    let choices = [
        format!("Connection         {connection}"),
        format!("Permissions        {}", app.settings.permission.label()),
        format!(
            "Computer Use      {}",
            if app.settings.computer_use {
                "on"
            } else {
                "off"
            }
        ),
    ];
    let mut lines = vec![muted("Runtime and OpenAI tunnel settings."), Line::from("")];
    for (index, label) in choices.into_iter().enumerate() {
        lines.push(choice(index == selected, label, ""));
    }
    lines.push(Line::from(""));
    lines.push(muted("↑↓ choose · Enter open/apply · Esc back"));
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn connection(frame: &mut Frame<'_>, app: &App, field: usize, tunnel_id: &str, api_key: &str) {
    let area = detail_layout(frame, "connection");
    let secret = if api_key.is_empty() {
        if app.paths.has_api_key() {
            "•••••••• (saved)"
        } else {
            "required"
        }
    } else {
        "•••••••• (new)"
    };
    let lines = vec![
        muted("Use the dedicated tunnel created for FKN Codex."),
        muted("The runtime API key is stored separately and never displayed again."),
        Line::from(""),
        field_line(field == 0, "Tunnel ID", tunnel_id),
        field_line(field == 1, "Runtime API key", secret),
        Line::from(""),
        choice(field == 2, "Save connection", ""),
        Line::from(""),
        Line::from(Span::styled(
            if app.status.starts_with("Error:") {
                app.status.clone()
            } else {
                String::new()
            },
            Style::new().red(),
        )),
        muted("Tab move · Enter next/save · Ctrl+S save · Esc back"),
    ];
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn field_line(selected: bool, label: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(if selected { "› " } else { "  " }, brand().bold()),
        Span::styled(format!("{label:<19}"), Style::new().dim()),
        Span::styled(
            value.to_string(),
            if selected {
                Style::new().bold()
            } else {
                Style::new()
            },
        ),
    ])
}

fn permissions(frame: &mut Frame<'_>, app: &App, selected: usize) {
    let area = detail_layout(frame, "permissions");
    let mut lines = vec![
        muted("Applied to the hidden Codex execution runtime."),
        Line::from(""),
    ];
    for (index, mode) in PermissionMode::ALL.into_iter().enumerate() {
        let suffix = if app.settings.permission == mode {
            "  current"
        } else {
            ""
        };
        let mut line = choice(index == selected, format!("{}{}", mode.label(), suffix), "");
        if mode == PermissionMode::DangerFullAccess {
            line = Line::from(
                line.spans
                    .into_iter()
                    .map(|span| Span::styled(span.content.into_owned(), Style::new().fg(WARNING)))
                    .collect::<Vec<_>>(),
            );
        }
        lines.push(line);
    }
    lines.push(Line::from(""));
    lines.push(muted("↑↓ choose · Enter apply · Esc back"));
    frame.render_widget(Paragraph::new(lines), area);
}

fn diagnostics(frame: &mut Frame<'_>, app: &mut App) {
    app.runtime.refresh();
    let area = detail_layout(frame, "diagnostics");
    let bridge_running = app.runtime.bridge_running();
    let tunnel_running = app.runtime.tunnel_running();
    let mut lines = vec![
        pair("Project", app.workspace.display().to_string()),
        pair("Bridge", if bridge_running { "ready" } else { "stopped" }),
        pair("Tunnel", if tunnel_running { "running" } else { "stopped" }),
        pair("MCP", app.runtime.mcp_url().unwrap_or("—")),
        pair("Tunnel health", app.runtime.health_url().unwrap_or("—")),
        pair("Codex home", app.paths.codex_home.display().to_string()),
        pair("Config", app.paths.config_dir.display().to_string()),
        Line::from(""),
        pair(
            "bridge bin",
            path_or_missing(app.binaries.bridge.as_deref()),
        ),
        pair("codex bin", path_or_missing(app.binaries.codex.as_deref())),
        pair(
            "auth shim",
            path_or_missing(app.binaries.auth_shim.as_deref()),
        ),
        pair(
            "tunnel bin",
            path_or_missing(app.binaries.tunnel.as_deref()),
        ),
        Line::from(""),
    ];
    if let Some(last) = app.runtime.logs().last() {
        lines.push(muted(format!("Last runtime event: {last}")));
    }
    lines.push(muted("Esc back · q quit"));
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn help(frame: &mut Frame<'_>) {
    let area = detail_layout(frame, "help");
    let lines = vec![
        strong("FKN Codex"),
        Line::from(""),
        Line::from("Start launches the local Codex runtime and OpenAI tunnel."),
        Line::from("Pause stops only the tunnel; shell/browser runtime state stays alive."),
        Line::from("Resume reconnects the same local runtime through the tunnel."),
        Line::from("Quit stops both tunnel and local runtime."),
        Line::from(""),
        muted("Create a dedicated tunnel in OpenAI Platform → Tunnels."),
        muted("Use a runtime API key for tunnel-client, not an admin key."),
        Line::from(""),
        muted("Esc back · q quit"),
    ];
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn path_or_missing(path: Option<&std::path::Path>) -> String {
    path.map(|path| path.display().to_string())
        .unwrap_or_else(|| "missing".to_string())
}
