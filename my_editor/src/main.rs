use std::{
    path::PathBuf,
    sync::Arc,
};

use anyhow::{Context, Result};
use arc_swap::{access::Map, ArcSwap};
use futures_util::StreamExt;

use helix_core::syntax;
use helix_view::{
    theme, Editor,
    graphics::{Rect, Color, Style, CursorKind},
    document::Mode,
};
use helix_term::keymap::{Keymaps, KeymapResult};
use helix_term::job::Jobs;
use helix_term::commands::Context as CmdContext;
use helix_term::commands;

use termina::Terminal as _;
use tui::{backend::TerminaBackend};

type TerminalBackend = TerminaBackend;
type Terminal = tui::terminal::Terminal<TerminalBackend>;
type TerminalEvent = termina::Event;

#[tokio::main]
async fn main() -> Result<()> {
    let config = helix_view::editor::Config::default();
    let terminal_config = tui::terminal::Config::from(&config);
    
    let mut terminal = Terminal::new(
        TerminaBackend::new(terminal_config)
            .context("failed to create terminal backend")?
    )?;
    terminal.claim()?;

    let area = terminal.size();

    let theme_loader = theme::Loader::new(&[PathBuf::from("../runtime/themes")]);
    let theme = theme_loader.default_theme(true);
    let config = helix_core::syntax::config::Configuration {
        language: vec![],
        language_server: Default::default(),
    };
    let lang_loader = syntax::Loader::new(config).unwrap(); // Dummy loader

    let config = Arc::new(ArcSwap::from_pointee(helix_view::editor::Config::default()));

    let (tx_auto_save, _rx_auto_save) = tokio::sync::mpsc::channel(1);
    let (tx_doc_colors, _rx_doc_colors) = tokio::sync::mpsc::channel(1);
    let (tx_pull_diags, _rx_pull_diags) = tokio::sync::mpsc::channel(1);
    let (tx_pull_all_diags, _rx_pull_all_diags) = tokio::sync::mpsc::channel(1);
    let (tx_sig_help, _rx_sig_help) = tokio::sync::mpsc::channel(1);
    let (tx_completion, _rx_completion) = tokio::sync::mpsc::channel(1);

    let handlers = helix_view::handlers::Handlers {
        completions: helix_view::handlers::completion::CompletionHandler::new(tx_completion),
        auto_save: tx_auto_save,
        document_colors: tx_doc_colors,
        pull_diagnostics: tx_pull_diags,
        pull_all_documents_diagnostics: tx_pull_all_diags,
        signature_hints: tx_sig_help,
        word_index: helix_view::handlers::word_index::Handler::spawn(),
    };

    let mut editor = Editor::new(
        area,
        Arc::new(theme_loader),
        Arc::new(ArcSwap::from_pointee(lang_loader)),
        Arc::new(Map::new(Arc::clone(&config), |config: &helix_view::editor::Config| config)),
        handlers,
    );

    editor.set_theme(theme);
    editor.new_file(helix_view::editor::Action::VerticalSplit);

    // Initial render
    terminal.clear()?;
    render(&mut editor, &mut terminal).await;

    // Event loop
    let reader = terminal.backend().terminal().event_reader();
    let mut events = termina::EventStream::new(reader, |event| {
        !event.is_escape() || matches!(event, termina::Event::Csi(termina::escape::csi::Csi::Mode(termina::escape::csi::Mode::ReportTheme(_))))
    });

    let mut jobs = Jobs::new();
    let mut keymaps = Keymaps::default();
    let mut on_next_key: Option<Box<dyn FnOnce(&mut CmdContext, helix_view::input::KeyEvent)>> = None;

    loop {
        tokio::select! {
            Some(event) = events.next() => {
                match event {
                    Ok(termina::Event::WindowResized(termina::WindowSize { rows, cols, .. })) => {
                        terminal.resize(Rect::new(0, 0, cols, rows))?;
                        let new_area = terminal.size();
                        editor.tree.resize(new_area);
                        terminal.clear()?;
                        render(&mut editor, &mut terminal).await;
                    }
                    Ok(termina::Event::Key(event)) if event.kind == termina::event::KeyEventKind::Press || event.kind == termina::event::KeyEventKind::Repeat => {
                        let key: helix_view::input::KeyEvent = event.into();
                        
                        let mut cx = CmdContext {
                            register: None,
                            count: None,
                            editor: &mut editor,
                            callback: Vec::new(),
                            on_next_key_callback: None,
                            jobs: &mut jobs,
                        };

                        if let Some(cb) = on_next_key.take() {
                            cb(&mut cx, key);
                        } else {
                            match keymaps.get(cx.editor.mode, key) {
                                KeymapResult::Matched(cmd) => cmd.execute(&mut cx),
                                KeymapResult::MatchedSequence(cmds) => {
                                    for cmd in cmds {
                                        cmd.execute(&mut cx);
                                    }
                                }
                                KeymapResult::NotFound => {
                                    if cx.editor.mode == Mode::Insert {
                                        if let Some(ch) = key.char() {
                                            commands::insert::insert_char(&mut cx, ch);
                                        }
                                    }
                                }
                                KeymapResult::Cancelled(pending) => {
                                    if cx.editor.mode == Mode::Insert {
                                        for ev in pending {
                                            match ev.char() {
                                                Some(ch) => commands::insert::insert_char(&mut cx, ch),
                                                None => {
                                                    if let KeymapResult::Matched(cmd) = keymaps.get(Mode::Insert, ev) {
                                                        cmd.execute(&mut cx);
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                        
                        if let Some((cb, _kind)) = cx.on_next_key_callback.take() {
                            on_next_key = Some(cb);
                        }
                        
                        terminal.clear()?;
                        render(&mut editor, &mut terminal).await;

                        if editor.should_close() {
                            terminal.restore()?;
                            break;
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    Ok(())
}

async fn render(editor: &mut Editor, terminal: &mut Terminal) {
    let area = terminal
        .autoresize()
        .expect("Unable to determine terminal size");

    let surface = terminal.current_buffer_mut();

    let bg = editor.theme.get("ui.background");
    surface.clear_with(area, bg);

    for (view, _is_focused) in editor.tree.views() {
        let doc = editor.document(view.doc).unwrap();
        // Since we aren't pulling in the full ui rendering compositor from helix_term,
        // we'll just render the document text very simply.
        
        let inner = view.inner_area(doc);
        let text = doc.text().slice(..);
        
        // Render very basic text lines
        for i in 0..inner.height {
            let line_index = i as usize; // Simplified
            if line_index < text.len_lines() {
                let line = text.line(line_index);
                let _ = surface.set_stringn(
                    inner.x,
                    inner.y + i,
                    line.to_string(),
                    inner.width as usize,
                    Style::default().fg(Color::White),
                );
            }
        }
    }

    let (view, doc) = helix_view::current!(editor);
    let text = doc.text().slice(..);
    let cursor_pos = doc.selection(view.id).primary().cursor(text);
    let cursor_line = text.char_to_line(cursor_pos);
    let cursor_char = cursor_pos - text.line_to_char(cursor_line);
    
    let inner = view.inner_area(doc);
    let draw_x = inner.x + cursor_char as u16;
    let draw_y = inner.y + cursor_line as u16;
    
    let kind = match editor.mode {
        Mode::Insert => CursorKind::Bar,
        _ => CursorKind::Block,
    };
    
    terminal.draw(Some((draw_x, draw_y)), kind).unwrap();
}
