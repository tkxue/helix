use std::{
    path::PathBuf,
    sync::Arc,
};

use anyhow::{Context as _, Result};
use arc_swap::{access::Map, ArcSwap};
// use futures_util::StreamExt; // unused

use helix_core::syntax;
use helix_view::{
    theme, Editor,
    graphics::{Color, Style, CursorKind},
    document::Mode,
};
use helix_term::keymap::{Keymaps, KeymapResult};
use helix_term::job::Jobs;
use helix_term::commands::Context as CmdContext;
use helix_term::commands;

use tui::backend::AlacrittyBackend;
use helix_view::input::VteEventParser;
use termina::Terminal as _;

use helix_core::syntax::{HighlightEvent, Highlighter};
use tui::text::{Span, Spans};

type TerminalBackend = AlacrittyBackend<std::io::Stdout>;
type Terminal = tui::terminal::Terminal<TerminalBackend>;

#[tokio::main]
async fn main() -> Result<()> {
    helix_loader::initialize_config_file(None);
    helix_loader::initialize_log_file(None);
    
    let mut platform_terminal = termina::PlatformTerminal::new()?;
    platform_terminal.enter_raw_mode()?;

    let mut terminal = Terminal::new(
        AlacrittyBackend::new(std::io::stdout())
            .context("failed to create terminal backend")?
    )?;
    terminal.claim()?;

    let area = terminal.size();

    let runtime_dir = helix_loader::runtime_dirs().first().expect("No runtime directory found");
    let theme_loader = theme::Loader::new(&[runtime_dir.join("themes")]);
    let theme = theme_loader.default_theme(true);
    
    let lang_config_path = runtime_dir.parent().unwrap().join("languages.toml");
    let lang_config: helix_core::syntax::config::Configuration = toml::from_str(
        &std::fs::read_to_string(&lang_config_path)
            .context(format!("failed to read languages.toml at {:?}", lang_config_path))?
    ).context("failed to parse languages.toml")?;
    
    let lang_loader = syntax::Loader::new(lang_config).unwrap();

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

    let mut editor_area = area;
    editor_area.height = editor_area.height.saturating_sub(1);

    let mut editor = Editor::new(
        editor_area,
        Arc::new(theme_loader),
        Arc::new(ArcSwap::from_pointee(lang_loader)),
        Arc::new(Map::new(Arc::clone(&config), |config: &helix_view::editor::Config| config)),
        handlers,
    );

    editor.set_theme(theme);
    
    let test_py_path = std::path::Path::new("my_editor/test.py");
    if test_py_path.exists() {
        editor.open(test_py_path, helix_view::editor::Action::VerticalSplit).expect("Failed to open test.py");
    } else {
        editor.new_file(helix_view::editor::Action::VerticalSplit);
    }

    // Initial render
    terminal.clear()?;
    render(&mut editor, &mut terminal).await;

    // Event loop
    // VTE Event loop setup
    let mut stdin = tokio::io::stdin();
    let mut buf = [0u8; 1024];
    let mut vte_parser = VteEventParser::new();

    let mut jobs = Jobs::new();
    let mut keymaps = Keymaps::default();
    let mut on_next_key: Option<Box<dyn FnOnce(&mut CmdContext, helix_view::input::KeyEvent)>> = None;
    let mut esc_timeout: Option<std::pin::Pin<Box<tokio::time::Sleep>>> = None;

    loop {
        tokio::select! {
            _ = async {
                if let Some(sleep) = esc_timeout.as_mut() {
                    sleep.await;
                } else {
                    futures_util::future::pending::<()>().await;
                }
            } => {
                esc_timeout = None;
                let parsed_events = vec![helix_view::input::Event::Key(helix_view::input::KeyEvent {
                    code: helix_view::input::KeyCode::Esc,
                    modifiers: helix_view::input::KeyModifiers::NONE,
                })];
                handle_events(parsed_events, &mut editor, &mut terminal, &mut keymaps, &mut jobs, &mut on_next_key).await;
            }
            res = tokio::io::AsyncReadExt::read(&mut stdin, &mut buf) => {
                match res {
                    Ok(n) if n > 0 => {
                        let mut input_bytes = &buf[..n];
                        
                        if input_bytes == [0x1B] {
                            // Start timeout for ESC
                            esc_timeout = Some(Box::pin(tokio::time::sleep(tokio::time::Duration::from_millis(20))));
                            continue;
                        }
                        
                        esc_timeout = None;
                        let parsed_events = vte_parser.advance(input_bytes);
                        if !handle_events(parsed_events, &mut editor, &mut terminal, &mut keymaps, &mut jobs, &mut on_next_key).await {
                            break;
                        }
                    }
                    _ => break,
                }
            }
        }
    }

    Ok(())
}

async fn handle_events(
    events: Vec<helix_view::input::Event>,
    editor: &mut Editor,
    terminal: &mut Terminal,
    keymaps: &mut Keymaps,
    jobs: &mut Jobs,
    on_next_key: &mut Option<Box<dyn FnOnce(&mut CmdContext, helix_view::input::KeyEvent)>>,
) -> bool {
    for ev in events {
        eprintln!("Event: {:?}", ev);
        let helix_view::input::Event::Key(key) = ev else { continue; };
        
        let mut cx = CmdContext {
            register: None,
            count: None,
            editor,
            callback: Vec::new(),
            on_next_key_callback: None,
            jobs,
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
            *on_next_key = Some(cb);
        }
        
        if editor.should_close() {
            return false;
        }
    }
    
    // Render after every event chunk
    terminal.clear().ok();
    render(editor, terminal).await;
    true
}

async fn render(editor: &mut Editor, terminal: &mut Terminal) {
    let area = terminal
        .autoresize()
        .expect("Unable to determine terminal size");

    let surface = terminal.current_buffer_mut();

    let bg = editor.theme.get("ui.background");
    surface.clear_with(area, bg);

    let (view, _is_focused) = editor.tree.views().next().unwrap();
    let doc = editor.document(view.doc).unwrap();
    let theme = &editor.theme;
    
    let inner = view.inner_area(doc);
    let text = doc.text().slice(..);
    
    let loader = editor.syn_loader.load();
    let mut highlighter = doc.syntax.as_ref().map(|syntax| {
        syntax.highlighter(text, &loader, 0..text.len_bytes() as u32)
    });

    let mut style = theme.get("ui.text");

    for i in 0..inner.height {
        let line_index = i as usize;
        if line_index < text.len_lines() {
            let line = text.line(line_index);
            let line_start_char = text.line_to_char(line_index);
            
            if let Some(ref mut highlighter) = highlighter {
                let mut x_offset = 0;
                let mut current_pos = line_start_char;
                
                // This is a VERY simplified rendering that doesn't handle overlapping spans perfectly
                // but should provide colors for a demo.
                let mut line_spans = Vec::new();
                
                // We need to advance the highlighter to the current line
                while highlighter.next_event_offset() < text.char_to_byte(line_start_char) as u32 {
                     highlighter.advance();
                }

                let line_end_byte = text.char_to_byte(text.line_to_char(line_index + 1).min(text.len_chars())) as u32;

                while highlighter.next_event_offset() < line_end_byte {
                    let next_event = highlighter.next_event_offset() as usize;
                    let next_event_char = text.byte_to_char(next_event).min(text.line_to_char(line_index + 1));
                    
                    if next_event_char > current_pos {
                        let span_text = text.slice(current_pos..next_event_char).to_string();
                        line_spans.push(Span::styled(span_text, style));
                        current_pos = next_event_char;
                    }
                    
                    let (event, highlights) = highlighter.advance();
                    match event {
                        HighlightEvent::Push => {
                            for h in highlights {
                                style = style.patch(theme.highlight(h));
                            }
                        }
                        HighlightEvent::Refresh => {
                            style = theme.get("ui.text");
                            for h in highlights {
                                style = style.patch(theme.highlight(h));
                            }
                        }
                    }
                }
                
                // Final span for the rest of the line
                let line_end_char = text.line_to_char(line_index + 1).min(text.len_chars());
                if line_end_char > current_pos {
                    let span_text = text.slice(current_pos..line_end_char).to_string();
                    line_spans.push(Span::styled(span_text, style));
                }

                let spans = Spans::from(line_spans);
                surface.set_spans(inner.x, inner.y + i, &spans, inner.width);
            } else {
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

    let cursor_pos = doc.selection(view.id).primary().cursor(text);
    let cursor_line = text.char_to_line(cursor_pos);
    let cursor_char = cursor_pos - text.line_to_char(cursor_line);
    
    let draw_x = inner.x + cursor_char as u16;
    let draw_y = inner.y + cursor_line as u16;
    
    let kind = match editor.mode {
        Mode::Insert => CursorKind::Bar,
        _ => CursorKind::Block,
    };

    // Status line
    let status = format!("-- {:?} --", editor.mode);
    surface.set_string(0, area.height - 1, status, Style::default().fg(Color::Yellow));

    terminal.draw(Some((draw_x, draw_y)), kind).unwrap();
}
