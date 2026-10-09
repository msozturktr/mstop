//! All colors live here; nothing is hardcoded elsewhere.

use ratatui::style::{Color, Modifier, Style};

pub const BG: Color = Color::Rgb(0x0d, 0x11, 0x17);
pub const PANEL_BG: Color = Color::Rgb(0x09, 0x0f, 0x17);
pub const BORDER: Color = Color::Rgb(0x1e, 0x2a, 0x3c);
pub const TEXT: Color = Color::Rgb(0xe6, 0xed, 0xf3);
pub const DIM: Color = Color::Rgb(0x8b, 0x9b, 0xb4);
pub const MUTED: Color = Color::Rgb(0x4b, 0x6a, 0x9b);
pub const SELECTION: Color = Color::Rgb(0x1c, 0x2a, 0x3b);

pub const ACCENT_CPU: Color = Color::Rgb(0x38, 0xbd, 0xf8);
pub const ACCENT_MEM: Color = Color::Rgb(0xa7, 0x8b, 0xfa);
pub const ACCENT_GPU: Color = Color::Rgb(0x5e, 0xea, 0xd4);
pub const ACCENT_DISK: Color = Color::Rgb(0xfb, 0x92, 0x3c);
pub const ACCENT_NET: Color = Color::Rgb(0xf4, 0x72, 0xb6);
/// Lighter shade of the network accent, used for the upload series.
pub const ACCENT_NET_UP: Color = Color::Rgb(0xfb, 0xcf, 0xe8);
pub const ACCENT_SENSORS: Color = Color::Rgb(0xfa, 0xcc, 0x15);
pub const ACCENT_SVC: Color = Color::Rgb(0x86, 0xef, 0xac);
pub const ACCENT_PROCS: Color = Color::Rgb(0x60, 0xa5, 0xfa);

pub const OK: Color = Color::Rgb(0x5e, 0xea, 0xd4);
pub const WARN: Color = Color::Rgb(0xfb, 0xbf, 0x24);
pub const CRIT: Color = Color::Rgb(0xf8, 0x71, 0x71);

/// Cache/buffers segment of the stacked RAM bar.
pub const CACHE: Color = Color::Rgb(0x5b, 0x4b, 0x9a);
/// Unfilled part of meters and the free segment of stacked bars.
pub const TRACK: Color = Color::Rgb(0x1e, 0x2a, 0x3c);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Sev {
    Ok,
    Warn,
    Crit,
}

impl Sev {
    pub fn from_value(v: f64, warn: f64, crit: f64) -> Sev {
        if v >= crit {
            Sev::Crit
        } else if v >= warn {
            Sev::Warn
        } else {
            Sev::Ok
        }
    }
    pub fn color(self) -> Color {
        match self {
            Sev::Ok => OK,
            Sev::Warn => WARN,
            Sev::Crit => CRIT,
        }
    }
    pub fn symbol(self) -> &'static str {
        match self {
            Sev::Ok => "✓",
            Sev::Warn => "▲",
            Sev::Crit => "✖",
        }
    }
    pub fn style(self) -> Style {
        Style::default().fg(self.color())
    }
}

pub fn text() -> Style {
    Style::default().fg(TEXT)
}
pub fn dim() -> Style {
    Style::default().fg(DIM)
}
pub fn muted() -> Style {
    Style::default().fg(MUTED)
}
pub fn bold(c: Color) -> Style {
    Style::default().fg(c).add_modifier(Modifier::BOLD)
}
