use std::io::{self, Write};
use helix_view::graphics::{Color, CursorKind, Modifier, Rect, Style, UnderlineStyle};
use crate::{backend::Backend, buffer::Cell, terminal::Config};

fn write_color(writer: &mut impl Write, color: Color, is_bg: bool) -> io::Result<()> {
    match color {
        Color::Reset => write!(writer, "\x1b[{}m", if is_bg { 49 } else { 39 }),
        Color::Black => write!(writer, "\x1b[{}m", if is_bg { 40 } else { 30 }),
        Color::Red => write!(writer, "\x1b[{}m", if is_bg { 41 } else { 31 }),
        Color::Green => write!(writer, "\x1b[{}m", if is_bg { 42 } else { 32 }),
        Color::Yellow => write!(writer, "\x1b[{}m", if is_bg { 43 } else { 33 }),
        Color::Blue => write!(writer, "\x1b[{}m", if is_bg { 44 } else { 34 }),
        Color::Magenta => write!(writer, "\x1b[{}m", if is_bg { 45 } else { 35 }),
        Color::Cyan => write!(writer, "\x1b[{}m", if is_bg { 46 } else { 36 }),
        Color::Gray => write!(writer, "\x1b[90m"),
        Color::LightRed => write!(writer, "\x1b[{}m", if is_bg { 101 } else { 91 }),
        Color::LightGreen => write!(writer, "\x1b[{}m", if is_bg { 102 } else { 92 }),
        Color::LightYellow => write!(writer, "\x1b[{}m", if is_bg { 103 } else { 93 }),
        Color::LightBlue => write!(writer, "\x1b[{}m", if is_bg { 104 } else { 94 }),
        Color::LightMagenta => write!(writer, "\x1b[{}m", if is_bg { 105 } else { 95 }),
        Color::LightCyan => write!(writer, "\x1b[{}m", if is_bg { 106 } else { 96 }),
        Color::LightGray => write!(writer, "\x1b[{}m", if is_bg { 47 } else { 37 }),
        Color::White => write!(writer, "\x1b[{}m", if is_bg { 107 } else { 97 }),
        Color::Indexed(i) => write!(writer, "\x1b[{};5;{}m", if is_bg { 48 } else { 38 }, i),
        Color::Rgb(r, g, b) => write!(writer, "\x1b[{};2;{};{};{}m", if is_bg { 48 } else { 38 }, r, g, b),
    }
}


pub struct AlacrittyBackend<W: Write> {
    writer: W,
    size: Rect,
}

impl<W: Write> AlacrittyBackend<W> {
    pub fn new(mut writer: W) -> Result<Self, io::Error> {
        // Just setting a dummy size for now; handle actual terminal size query later
        Ok(Self {
            writer,
            size: Rect::new(0, 0, 80, 24),
        })
    }
}

impl<W: Write> Backend for AlacrittyBackend<W> {
    fn claim(&mut self) -> Result<(), io::Error> {
        // Enter alternate screen and enable raw mode
        write!(self.writer, "\x1b[?1049h")?;
        self.writer.flush()
    }

    fn reconfigure(&mut self, _config: Config) -> Result<(), io::Error> {
        Ok(())
    }

    fn restore(&mut self) -> Result<(), io::Error> {
        // Leave alternate screen
        write!(self.writer, "\x1b[?1049l")?;
        self.writer.flush()
    }

    fn draw<'a, I>(&mut self, content: I) -> Result<(), io::Error>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        for (x, y, cell) in content {
            // Move cursor
            write!(self.writer, "\x1b[{};{}H", y + 1, x + 1)?;

            // Render modifiers
            if cell.modifier.contains(Modifier::BOLD) {
                write!(self.writer, "\x1b[1m")?;
            }
            if cell.modifier.contains(Modifier::ITALIC) {
                write!(self.writer, "\x1b[3m")?;
            }
            if cell.modifier.contains(Modifier::REVERSED) {
                write!(self.writer, "\x1b[7m")?;
            }

            // Colors
            write_color(&mut self.writer, cell.fg, false)?;
            write_color(&mut self.writer, cell.bg, true)?;

            // Write symbol
            write!(self.writer, "{}", cell.symbol)?;

            // Reset
            write!(self.writer, "\x1b[0m")?;
        }
        Ok(())
    }

    fn hide_cursor(&mut self) -> Result<(), io::Error> {
        write!(self.writer, "\x1b[?25l")
    }

    fn show_cursor(&mut self, _kind: CursorKind) -> Result<(), io::Error> {
        write!(self.writer, "\x1b[?25h")
    }

    fn set_cursor(&mut self, x: u16, y: u16) -> Result<(), io::Error> {
        write!(self.writer, "\x1b[{};{}H", y + 1, x + 1)
    }

    fn clear(&mut self) -> Result<(), io::Error> {
        write!(self.writer, "\x1b[2J")
    }

    fn size(&self) -> Result<Rect, io::Error> {
        Ok(self.size)
    }

    fn flush(&mut self) -> Result<(), io::Error> {
        self.writer.flush()
    }

    fn supports_true_color(&self) -> bool {
        true // We can assume alacritty backend targets true color
    }
    
    fn get_theme_mode(&self) -> Option<helix_view::theme::Mode> {
        None
    }
}
