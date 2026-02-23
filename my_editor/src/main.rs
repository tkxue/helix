use std::sync::Arc;

use anyhow::{Context as _, Result};
use arc_swap::{access::Map, ArcSwap};
use futures_util::StreamExt;

use helix_core::syntax;
use helix_view::{theme, Editor};
use helix_term::config::Config;
use helix_term::compositor::Compositor;
use helix_term::keymap::Keymaps;
use helix_term::job::Jobs;
use helix_term::ui::EditorView;
use helix_term::handlers;

use tui::backend::AlacrittyBackend;
use helix_view::input::VteEventParser;
use termina::Terminal as _;

type TerminalBackend = AlacrittyBackend<std::io::Stdout>;
type Terminal = tui::terminal::Terminal<TerminalBackend>;

#[tokio::main]
async fn main() -> Result<()> {
    helix_loader::initialize_config_file(None);
    helix_loader::initialize_log_file(None);

    // --- Terminal setup ---
    let mut platform_terminal = termina::PlatformTerminal::new()?;
    platform_terminal.enter_raw_mode()?;

    let mut terminal = Terminal::new(
        AlacrittyBackend::new(std::io::stdout())
            .context("failed to create terminal backend")?,
    )?;
    terminal.claim()?;

    let area = terminal.size();

    // --- Theme + syntax loader ---
    let runtime_dir = helix_loader::runtime_dirs()
        .first()
        .expect("No runtime directory found")
        .clone();
    let theme_loader = theme::Loader::new(&[runtime_dir.join("themes")]);
    let theme = theme_loader.default_theme(true);

    let lang_config_path = runtime_dir.parent().unwrap().join("languages.toml");
    let lang_config: helix_core::syntax::config::Configuration = toml::from_str(
        &std::fs::read_to_string(&lang_config_path)
            .context(format!("failed to read languages.toml at {:?}", lang_config_path))?,
    )
    .context("failed to parse languages.toml")?;

    let lang_loader = syntax::Loader::new(lang_config).unwrap();
    let lang_loader = Arc::new(ArcSwap::from_pointee(lang_loader));

    // --- Config: helix_term::config::Config (includes keymap + editor config) ---
    let config = Arc::new(ArcSwap::from_pointee(Config::default()));

    // --- Jobs: MUST be created before handlers::setup so JOB_QUEUE is initialized ---
    let mut jobs = Jobs::new();

    // --- Handlers: spawns async CompletionHandler, SignatureHelpHandler, hooks, etc. ---
    let handlers = handlers::setup(config.clone());

    // --- Editor ---
    let mut editor_area = area;
    editor_area.height = editor_area.height.saturating_sub(1);

    let mut editor = Editor::new(
        editor_area,
        Arc::new(theme_loader),
        lang_loader,
        Arc::new(Map::new(Arc::clone(&config), |c: &Config| &c.editor)),
        handlers,
    );

    editor.set_theme(theme);

    // --- Compositor + EditorView ---
    // EditorView owns completion: Option<Completion> and handles completion popup rendering.
    let mut compositor = Compositor::new(area);
    let editor_view = Box::new(EditorView::new(Keymaps::default()));
    compositor.push(editor_view);

    // --- Open file ---
    let test_py_path = std::path::Path::new("my_editor/test.py");
    if test_py_path.exists() {
        editor
            .open(test_py_path, helix_view::editor::Action::VerticalSplit)
            .expect("Failed to open test.py");
    } else {
        editor.new_file(helix_view::editor::Action::VerticalSplit);
    }

    // Initial render
    terminal.clear()?;
    render(&mut editor, &mut compositor, &mut jobs, &mut terminal);

    // --- Event loop ---
    let mut stdin = tokio::io::stdin();
    let mut buf = [0u8; 1024];
    let mut vte_parser = VteEventParser::new();
    let mut esc_timeout: Option<std::pin::Pin<Box<tokio::time::Sleep>>> = None;

    loop {
        if editor.should_close() {
            break;
        }

        tokio::select! {
            // ESC timeout: disambiguate lone ESC from ESC-sequences
            _ = async {
                if let Some(sleep) = esc_timeout.as_mut() {
                    sleep.await;
                } else {
                    futures_util::future::pending::<()>().await;
                }
            } => {
                esc_timeout = None;
                let key = helix_view::input::KeyEvent {
                    code: helix_view::input::KeyCode::Esc,
                    modifiers: helix_view::input::KeyModifiers::NONE,
                };
                handle_key(&helix_view::input::Event::Key(key), &mut editor, &mut compositor, &mut jobs);
                render(&mut editor, &mut compositor, &mut jobs, &mut terminal);
            }

            // Raw terminal input
            res = tokio::io::AsyncReadExt::read(&mut stdin, &mut buf) => {
                match res {
                    Ok(n) if n > 0 => {
                        let input_bytes = &buf[..n];

                        if input_bytes == [0x1B] {
                            esc_timeout = Some(Box::pin(tokio::time::sleep(
                                tokio::time::Duration::from_millis(20),
                            )));
                            continue;
                        }

                        esc_timeout = None;
                        let parsed_events = vte_parser.advance(input_bytes);
                        for ev in parsed_events {
                            handle_key(&ev, &mut editor, &mut compositor, &mut jobs);
                        }
                        render(&mut editor, &mut compositor, &mut jobs, &mut terminal);
                    }
                    _ => break,
                }
            }

            // Async job callbacks (completion results, LSP write responses, etc.)
            Some(callback) = jobs.callbacks.recv() => {
                jobs.handle_callback(&mut editor, &mut compositor, Ok(Some(callback)));
                render(&mut editor, &mut compositor, &mut jobs, &mut terminal);
            }

            // Wait-futures (jobs that must complete before quitting)
            Some(callback) = jobs.wait_futures.next() => {
                jobs.handle_callback(&mut editor, &mut compositor, callback);
                render(&mut editor, &mut compositor, &mut jobs, &mut terminal);
            }

            // Editor events: LSP messages, document saves, redraw requests, idle timer
            event = editor.wait_event() => {
                use helix_view::editor::EditorEvent;
                match event {
                    EditorEvent::LanguageServerMessage((id, call)) => {
                        handle_lsp_message(&mut editor, &mut compositor, &mut jobs, call, id).await;
                        render(&mut editor, &mut compositor, &mut jobs, &mut terminal);
                    }
                    EditorEvent::DocumentSaved(_) | EditorEvent::Redraw => {
                        render(&mut editor, &mut compositor, &mut jobs, &mut terminal);
                    }
                    EditorEvent::IdleTimer => {
                        editor.clear_idle_timer();
                        let mut cx = helix_term::compositor::Context {
                            editor: &mut editor,
                            jobs: &mut jobs,
                            scroll: None,
                        };
                        compositor.handle_event(&helix_view::input::Event::IdleTimeout, &mut cx);
                        render(&mut editor, &mut compositor, &mut jobs, &mut terminal);
                    }
                    _ => {}
                }
            }
        }
    }

    Ok(())
}

/// Route a single key event through the compositor (handles keymaps, completion popup,
/// PostInsertChar / PostCommand hooks, etc.)
fn handle_key(
    event: &helix_view::input::Event,
    editor: &mut Editor,
    compositor: &mut Compositor,
    jobs: &mut Jobs,
) {
    let mut cx = helix_term::compositor::Context {
        editor,
        jobs,
        scroll: None,
    };
    compositor.handle_event(event, &mut cx);
}

/// Render: delegate entirely to the compositor so that EditorView renders syntax
/// highlighting, the completion popup, the status line, etc.
fn render(
    editor: &mut Editor,
    compositor: &mut Compositor,
    jobs: &mut Jobs,
    terminal: &mut Terminal,
) {
    let area = terminal
        .autoresize()
        .expect("Unable to determine terminal size");

    // Drain any synchronous callbacks before rendering (some commands push callbacks
    // that must be executed before the compositor state is consistent).
    while let Ok(cb) = jobs.callbacks.try_recv() {
        jobs.handle_callback(editor, compositor, Ok(Some(cb)));
    }

    let surface = terminal.current_buffer_mut();
    let bg = editor.theme.get("ui.background");
    surface.clear_with(area, bg);

    let mut cx = helix_term::compositor::Context {
        editor,
        jobs,
        scroll: None,
    };
    compositor.render(area, surface, &mut cx);

    let (pos, kind) = compositor.cursor(area, cx.editor);
    let pos = pos.map(|p| (p.col as u16, p.row as u16));
    terminal.draw(pos, kind).unwrap();
}

/// Minimal LSP message handler: routes language server messages from
/// `editor.wait_event()` back to the editor and compositor.
///
/// This mirrors the relevant branches of `Application::handle_language_server_message`.
async fn handle_lsp_message(
    editor: &mut Editor,
    compositor: &mut Compositor,
    jobs: &mut Jobs,
    call: helix_lsp::Call,
    server_id: helix_lsp::LanguageServerId,
) {
    use helix_lsp::{Call, Notification};

    match call {
        Call::Notification(helix_lsp::jsonrpc::Notification { method, params, .. }) => {
            let notification = match Notification::parse(&method, params) {
                Ok(n) => n,
                Err(_) => return,
            };
            match notification {
                Notification::Initialized => {
                    if let Some(ls) = editor.language_server_by_id(server_id) {
                        if let Some(config) = ls.config() {
                            ls.did_change_configuration(config.clone());
                        }
                    }
                    helix_event::dispatch(helix_view::events::LanguageServerInitialized {
                        editor,
                        server_id,
                    });
                }
                Notification::PublishDiagnostics(params) => {
                    let uri = match helix_core::Uri::try_from(params.uri) {
                        Ok(u) => u,
                        Err(e) => { log::error!("{e}"); return; }
                    };
                    let provider = helix_core::diagnostic::DiagnosticProvider::Lsp {
                        server_id,
                        identifier: None,
                    };
                    editor.handle_lsp_diagnostics(
                        &provider,
                        uri,
                        params.version,
                        params.diagnostics,
                    );
                }
                Notification::ShowMessage(params) => {
                    editor.set_status(params.message);
                }
                Notification::LogMessage(params) => {
                    log::info!("window/logMessage: {:?}", params);
                }
                Notification::Exit => {
                    editor.set_status("Language server exited");
                    for diags in editor.diagnostics.values_mut() {
                        diags.retain(|(_, provider)| {
                            provider.language_server_id() != Some(server_id)
                        });
                    }
                    editor.diagnostics.retain(|_, diags| !diags.is_empty());
                    for doc in editor.documents_mut() {
                        doc.clear_diagnostics_for_language_server(server_id);
                    }
                    helix_event::dispatch(helix_view::events::LanguageServerExited {
                        editor,
                        server_id,
                    });
                    editor.language_servers.remove_by_id(server_id);
                }
                _ => {}
            }
        }
        Call::MethodCall(helix_lsp::jsonrpc::MethodCall { method, params, id, .. }) => {
            use helix_lsp::MethodCall;
            let reply = match MethodCall::parse(&method, params) {
                Err(_) => Err(helix_lsp::jsonrpc::Error {
                    code: helix_lsp::jsonrpc::ErrorCode::MethodNotFound,
                    message: format!("Method not found: {method}"),
                    data: None,
                }),
                Ok(MethodCall::WorkspaceFolders) => {
                    if let Some(ls) = editor.language_server_by_id(server_id) {
                        Ok(serde_json::json!(&*ls.workspace_folders().await))
                    } else {
                        return;
                    }
                }
                Ok(MethodCall::WorkspaceConfiguration(params)) => {
                    if let Some(ls) = editor.language_server_by_id(server_id) {
                        let result: Vec<_> = params
                            .items
                            .iter()
                            .map(|item| {
                                let mut config = ls.config()?;
                                if let Some(section) = item.section.as_ref() {
                                    if !section.is_empty() {
                                        for part in section.split('.') {
                                            config = config.get(part)?;
                                        }
                                    }
                                }
                                Some(config)
                            })
                            .collect();
                        Ok(serde_json::json!(result))
                    } else {
                        return;
                    }
                }
                Ok(_) => Ok(serde_json::Value::Null),
            };
            if let Some(ls) = editor.language_server_by_id(server_id) {
                ls.reply(id, reply).ok();
            }
        }
        _ => {}
    }
}
