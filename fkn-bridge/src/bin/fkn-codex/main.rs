mod runtime;
mod settings;
mod ui;

use anyhow::Context as _;
use anyhow::Result;
use clap::Parser;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use runtime::{Binaries, RuntimeController};
use settings::{AppPaths, PermissionMode, Settings, valid_tunnel_id};
use std::io;
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Debug, Parser)]
#[command(
    name = "fkn-codex",
    about = "Local Codex runtime connected to ChatGPT through OpenAI Secure MCP Tunnel"
)]
struct Args {
    /// Project workspace. Defaults to the current directory.
    #[arg(long)]
    workspace: Option<PathBuf>,

    /// Override the FKN Codex application/config directory (primarily for tests).
    #[arg(long, hide = true)]
    app_dir: Option<PathBuf>,
}

#[derive(Clone, Debug)]
enum View {
    Home,
    Settings {
        selected: usize,
    },
    Connection {
        field: usize,
        tunnel_id: String,
        api_key: String,
    },
    Permissions {
        selected: usize,
    },
    Diagnostics,
    Help,
}

struct App {
    workspace: PathBuf,
    paths: AppPaths,
    settings: Settings,
    binaries: Binaries,
    runtime: RuntimeController,
    view: View,
    home_selected: usize,
    status: String,
}

impl App {
    fn load(args: Args) -> Result<Self> {
        let workspace = match args.workspace {
            Some(path) => std::fs::canonicalize(&path)
                .with_context(|| format!("resolve workspace {}", path.display()))?,
            None => std::fs::canonicalize(std::env::current_dir()?)?,
        };
        let paths = AppPaths::discover(args.app_dir)?;
        let settings = paths.load_settings()?;
        let binaries = Binaries::discover();
        let runtime = RuntimeController::new(paths.state_dir.clone())?;
        let status = if valid_tunnel_id(&settings.tunnel_id) && paths.has_api_key() {
            "Ready. Start when you want ChatGPT to connect.".to_string()
        } else {
            "Set up the new FKN Codex tunnel first.".to_string()
        };
        Ok(Self {
            workspace,
            paths,
            settings,
            binaries,
            runtime,
            view: View::Home,
            home_selected: 0,
            status,
        })
    }

    fn is_configured(&self) -> bool {
        valid_tunnel_id(&self.settings.tunnel_id) && self.paths.has_api_key()
    }

    fn primary_action(&mut self) {
        if !self.is_configured() {
            self.open_connection();
            return;
        }

        let result = if self.runtime.tunnel_running() {
            self.runtime.pause();
            self.status = "Paused. Local Codex runtime is still alive.".to_string();
            Ok(())
        } else if self.runtime.bridge_running() {
            self.runtime
                .resume_tunnel(&self.paths, &self.settings, &self.binaries)
                .map(|_| self.status = "Tunnel resumed.".to_string())
        } else {
            self.runtime
                .start_all(&self.workspace, &self.paths, &self.settings, &self.binaries)
                .map(|_| self.status = "OpenAI tunnel started.".to_string())
        };

        if let Err(error) = result {
            self.status = format!("Error: {error:#}");
        }
    }

    fn open_connection(&mut self) {
        self.view = View::Connection {
            field: 0,
            tunnel_id: self.settings.tunnel_id.clone(),
            api_key: String::new(),
        };
    }

    fn save_connection(&mut self, tunnel_id: String, api_key: String) -> Result<()> {
        let tunnel_id = tunnel_id.trim();
        anyhow::ensure!(
            valid_tunnel_id(tunnel_id),
            "Tunnel ID must start with `tunnel_` and include an identifier"
        );
        anyhow::ensure!(
            self.paths.has_api_key() || !api_key.trim().is_empty(),
            "Runtime API key is required"
        );

        self.settings.tunnel_id = tunnel_id.to_string();
        self.paths.save_settings(&self.settings)?;
        if !api_key.trim().is_empty() {
            self.paths.save_api_key(api_key.trim())?;
        }
        if self.runtime.tunnel_running() {
            self.runtime.pause();
        }
        self.status = "Connection saved. Start or resume to use the new tunnel.".to_string();
        self.view = View::Settings { selected: 0 };
        Ok(())
    }

    fn apply_permission(&mut self, mode: PermissionMode) -> Result<()> {
        if self.settings.permission != mode {
            self.settings.permission = mode;
            self.paths.save_settings(&self.settings)?;
            if self.runtime.bridge_running() {
                self.runtime.stop();
                self.status =
                    "Permission saved. Runtime stopped; Start applies the new sandbox.".to_string();
            } else {
                self.status = "Permission saved.".to_string();
            }
        }
        self.view = View::Settings { selected: 1 };
        Ok(())
    }

    fn toggle_computer_use(&mut self) -> Result<()> {
        self.settings.computer_use = !self.settings.computer_use;
        self.paths.save_settings(&self.settings)?;
        if self.runtime.bridge_running() {
            self.runtime.stop();
            self.status =
                "Computer Use setting saved. Runtime stopped; Start applies it.".to_string();
        } else {
            self.status = "Computer Use setting saved.".to_string();
        }
        Ok(())
    }

    fn handle_paste(&mut self, text: &str) {
        let View::Connection {
            field,
            tunnel_id,
            api_key,
        } = &mut self.view
        else {
            return;
        };
        let cleaned = text.replace(['\r', '\n'], "");
        match *field {
            0 => tunnel_id.push_str(&cleaned),
            1 => api_key.push_str(&cleaned),
            _ => {}
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> Result<bool> {
        if key.code == KeyCode::Char('q') && !matches!(self.view, View::Connection { .. }) {
            return Ok(false);
        }
        if key.code == KeyCode::Char('?') && !matches!(self.view, View::Connection { .. }) {
            self.view = View::Help;
            return Ok(true);
        }

        match self.view.clone() {
            View::Home => match key.code {
                KeyCode::Up => self.home_selected = self.home_selected.saturating_sub(1),
                KeyCode::Down => self.home_selected = (self.home_selected + 1).min(2),
                KeyCode::Char('s') => self.view = View::Settings { selected: 0 },
                KeyCode::Char('d') => self.view = View::Diagnostics,
                KeyCode::Enter => match self.home_selected {
                    0 => self.primary_action(),
                    1 => self.view = View::Settings { selected: 0 },
                    2 => self.view = View::Diagnostics,
                    _ => {}
                },
                _ => {}
            },
            View::Settings { mut selected } => match key.code {
                KeyCode::Esc => self.view = View::Home,
                KeyCode::Up => {
                    selected = selected.saturating_sub(1);
                    self.view = View::Settings { selected };
                }
                KeyCode::Down => {
                    selected = (selected + 1).min(2);
                    self.view = View::Settings { selected };
                }
                KeyCode::Enter => match selected {
                    0 => self.open_connection(),
                    1 => {
                        let current = PermissionMode::ALL
                            .iter()
                            .position(|mode| *mode == self.settings.permission)
                            .unwrap_or(0);
                        self.view = View::Permissions { selected: current };
                    }
                    2 => self.toggle_computer_use()?,
                    _ => {}
                },
                _ => {}
            },
            View::Connection {
                mut field,
                mut tunnel_id,
                mut api_key,
            } => {
                if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
                    if let Err(error) = self.save_connection(tunnel_id, api_key) {
                        self.status = format!("Error: {error:#}");
                    }
                    return Ok(true);
                }
                match key.code {
                    KeyCode::Esc => self.view = View::Settings { selected: 0 },
                    KeyCode::Tab | KeyCode::Down => field = (field + 1) % 3,
                    KeyCode::BackTab | KeyCode::Up => field = (field + 2) % 3,
                    KeyCode::Enter if field == 2 => {
                        if let Err(error) = self.save_connection(tunnel_id, api_key) {
                            self.status = format!("Error: {error:#}");
                        }
                        return Ok(true);
                    }
                    KeyCode::Enter => field = (field + 1).min(2),
                    KeyCode::Backspace if field < 2 => {
                        if field == 0 {
                            tunnel_id.pop();
                        } else {
                            api_key.pop();
                        }
                    }
                    KeyCode::Char(character)
                        if field < 2 && !key.modifiers.contains(KeyModifiers::CONTROL) =>
                    {
                        if field == 0 {
                            tunnel_id.push(character);
                        } else {
                            api_key.push(character);
                        }
                    }
                    _ => {}
                }
                self.view = View::Connection {
                    field,
                    tunnel_id,
                    api_key,
                };
            }
            View::Permissions { mut selected } => match key.code {
                KeyCode::Esc => self.view = View::Settings { selected: 1 },
                KeyCode::Up => {
                    selected = selected.saturating_sub(1);
                    self.view = View::Permissions { selected };
                }
                KeyCode::Down => {
                    selected = (selected + 1).min(PermissionMode::ALL.len() - 1);
                    self.view = View::Permissions { selected };
                }
                KeyCode::Enter => self.apply_permission(PermissionMode::ALL[selected])?,
                _ => {}
            },
            View::Diagnostics | View::Help => {
                if key.code == KeyCode::Esc {
                    self.view = View::Home;
                }
            }
        }
        Ok(true)
    }
}

struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen)?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

fn run_tui(mut app: App) -> Result<()> {
    let _guard = TerminalGuard::enter()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let tick_rate = Duration::from_millis(150);
    let mut last_tick = Instant::now();
    loop {
        terminal.draw(|frame| ui::draw(frame, &mut app))?;

        let timeout = tick_rate.saturating_sub(last_tick.elapsed());
        if event::poll(timeout)? {
            match event::read()? {
                Event::Key(key) if key.kind == event::KeyEventKind::Press => {
                    if !app.handle_key(key)? {
                        break;
                    }
                }
                Event::Paste(text) => app.handle_paste(&text),
                Event::Resize(_, _) => {}
                _ => {}
            }
        }
        if last_tick.elapsed() >= tick_rate {
            app.runtime.refresh();
            app.runtime
                .maintain(&app.workspace, &app.paths, &app.settings, &app.binaries);
            last_tick = Instant::now();
        }
    }

    app.runtime.stop();
    Ok(())
}

fn main() -> Result<()> {
    let args = Args::parse();
    let app = App::load(args)?;
    run_tui(app)
}

#[cfg(test)]
mod tests;
