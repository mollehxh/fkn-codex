use super::*;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Fixture {
    root: PathBuf,
    app: App,
}

impl Fixture {
    fn new(configured: bool) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "fkn-codex-tui-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let workspace = root.join("workspace");
        let app_dir = root.join("app");
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(&app_dir).unwrap();
        let mut app = App::load(Args {
            workspace: Some(workspace),
            app_dir: Some(app_dir),
        })
        .unwrap();
        if configured {
            app.settings.tunnel_id = "tunnel_test".to_string();
            app.paths.save_settings(&app.settings).unwrap();
            app.paths.save_api_key("secret-test-key").unwrap();
            app.status = "Ready. Start when you want ChatGPT to connect.".to_string();
        }
        Self { root, app }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.app.runtime.stop();
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn render(app: &mut App, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| ui::draw(frame, app)).unwrap();
    let buffer = terminal.backend().buffer();
    buffer
        .content
        .chunks(width.max(1) as usize)
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

fn key(app: &mut App, code: KeyCode) {
    app.handle_key(KeyEvent::new(code, KeyModifiers::NONE))
        .unwrap();
}

#[test]
fn home_is_compact_and_has_no_activity_panel() {
    let mut fixture = Fixture::new(true);
    let screen = render(&mut fixture.app, 90, 26);
    assert!(screen.contains("FKN Codex"));
    assert!(screen.contains("Start"));
    assert!(screen.contains("Settings"));
    assert!(screen.contains("Diagnostics"));
    assert!(screen.contains("Workspace write"));
    assert!(!screen.contains("Activity"));
    assert!(!screen.contains("task plan"));
}

#[test]
fn first_run_goes_directly_to_connection_setup() {
    let mut fixture = Fixture::new(false);
    assert!(!fixture.app.is_configured());
    key(&mut fixture.app, KeyCode::Enter);
    assert!(matches!(fixture.app.view, View::Connection { .. }));
    let screen = render(&mut fixture.app, 90, 26);
    assert!(screen.contains("Use the dedicated tunnel created for FKN Codex."));
    assert!(screen.contains("Tunnel ID"));
    assert!(screen.contains("Runtime API key"));
}

#[test]
fn connection_secret_is_persisted_but_never_rendered() {
    let mut fixture = Fixture::new(false);
    fixture.app.open_connection();
    if let View::Connection {
        tunnel_id,
        api_key,
        field,
    } = &mut fixture.app.view
    {
        *tunnel_id = "tunnel_new_fkn".to_string();
        *api_key = "super-secret-runtime-key".to_string();
        *field = 1;
    }
    let screen = render(&mut fixture.app, 90, 26);
    assert!(!screen.contains("super-secret-runtime-key"));

    let (tunnel_id, api_key) = match fixture.app.view.clone() {
        View::Connection {
            tunnel_id, api_key, ..
        } => (tunnel_id, api_key),
        _ => unreachable!(),
    };
    fixture.app.save_connection(tunnel_id, api_key).unwrap();
    assert!(fixture.app.is_configured());
    assert_eq!(
        fs::read_to_string(&fixture.app.paths.api_key_file).unwrap(),
        "super-secret-runtime-key"
    );
    assert!(
        !fs::read_to_string(&fixture.app.paths.settings_file)
            .unwrap()
            .contains("super-secret-runtime-key")
    );
}

#[test]
fn settings_expose_only_runtime_controls_we_need() {
    let mut fixture = Fixture::new(true);
    fixture.app.view = View::Settings { selected: 0 };
    let screen = render(&mut fixture.app, 90, 26);
    assert!(screen.contains("Connection"));
    assert!(screen.contains("Permissions"));
    assert!(screen.contains("Computer Use"));
    assert!(!screen.contains("Activity"));
    assert!(!screen.contains("Agents"));
}

#[test]
fn tunnel_id_validation_is_specific() {
    assert!(valid_tunnel_id("tunnel_abc"));
    assert!(!valid_tunnel_id(""));
    assert!(!valid_tunnel_id("abc"));
    assert!(!valid_tunnel_id("tunnel_"));
}
