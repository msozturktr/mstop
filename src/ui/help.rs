//! Help overlay and kill-confirm dialog.

use super::theme::{self, *};
use super::widgets::panel_block;
use crate::app::KillDialog;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height - h) / 2,
        w,
        h,
    )
}

pub fn draw_help(f: &mut Frame, area: Rect) {
    let keys: [(&str, &str); 15] = [
        ("q / Ctrl-C", "Quit"),
        ("F1 / ?", "Toggle this help"),
        ("Up Down PgUp PgDn Home End", "Move process selection"),
        ("/", "Filter processes (Enter keeps, Esc clears)"),
        ("s", "Cycle sort key: CPU, memory, PID, name, disk I/O"),
        ("S", "Reverse sort direction"),
        ("t", "Toggle process tree"),
        ("k", "Kill selected process (asks first)"),
        ("h", "History span of all graphs: 60 s, 10 min, 1 h, 24 h"),
        ("Space", "Pause / resume updates"),
        ("Tab / Shift-Tab", "Next / previous panel"),
        (
            "1 2 3 4 5 6 7 8",
            "Focus CPU, Memory, GPU, Network, Sensors, Disks, Procs, Services",
        ),
        ("✓ ▲ ✖", "Status: ok, warning, critical"),
        ("", ""),
        ("Esc", "Close dialogs"),
    ];
    let r = centered(area, 76, keys.len() as u16 + 4);
    f.render_widget(Clear, r);
    let block = panel_block(0, "Help", None, ACCENT_CPU, true, r.width);
    let inner = block.inner(r);
    f.render_widget(block, r);
    let lines: Vec<Line> = keys
        .iter()
        .map(|(k, d)| {
            Line::from(vec![
                Span::styled(format!("{k:<30}"), theme::bold(ACCENT_CPU)),
                Span::styled(*d, theme::text()),
            ])
        })
        .collect();
    f.render_widget(Paragraph::new(lines), inner);
}

pub fn draw_kill(f: &mut Frame, area: Rect, d: &KillDialog) {
    let r = centered(area, 72, 5);
    f.render_widget(Clear, r);
    let block = panel_block(0, "Confirm", None, CRIT, true, r.width);
    let inner = block.inner(r);
    f.render_widget(block, r);
    let lines = vec![
        Line::styled(
            format!("Send SIGTERM to {} (PID {})?", d.name, d.pid),
            theme::bold(WARN),
        ),
        Line::from(vec![
            Span::styled("[y]", theme::bold(OK)),
            Span::styled(" yes  ", theme::text()),
            Span::styled("[K]", theme::bold(CRIT)),
            Span::styled(" SIGKILL  ", theme::text()),
            Span::styled("[n]", theme::bold(ACCENT_CPU)),
            Span::styled(" cancel", theme::text()),
        ]),
    ];
    f.render_widget(Paragraph::new(lines), inner);
}
