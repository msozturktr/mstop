//! Bottom key-hint line.

use super::theme::{self, *};
use crate::app::App;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

const HINTS: [(&str, &str); 10] = [
    ("F1", "Help"),
    ("/", "Filter"),
    ("s", "Sort"),
    ("t", "Tree"),
    ("k", "Kill"),
    ("h", "History span"),
    ("Tab", "Next panel"),
    ("1-8", "Panel"),
    ("Space", "Pause"),
    ("q", "Quit"),
];

pub fn draw(f: &mut Frame, area: Rect, app: &App) {
    let right = match &app.message {
        Some((m, at)) if at.elapsed().as_secs() < 6 => m.clone(),
        _ if app.paused => "⏸ PAUSED".to_string(),
        _ => format!("span {} · {} ms", app.span.label(), app.interval_ms),
    };
    let right_w = right.chars().count() + 1;
    let budget = (area.width as usize).saturating_sub(right_w + 1);
    let mut spans = vec![Span::raw(" ")];
    let mut used = 1;
    for (k, label) in HINTS {
        let w = k.chars().count() + label.chars().count() + 5;
        if used + w > budget {
            break;
        }
        spans.push(Span::styled(format!("[{k}]"), theme::bold(ACCENT_CPU)));
        spans.push(Span::styled(format!(" {label}  "), theme::dim()));
        used += w;
    }
    let pad = (area.width as usize).saturating_sub(used + right_w);
    spans.push(Span::raw(" ".repeat(pad)));
    let style = if app.paused {
        theme::bold(WARN)
    } else {
        theme::muted()
    };
    spans.push(Span::styled(right, style));
    f.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(BG)),
        area,
    );
}
