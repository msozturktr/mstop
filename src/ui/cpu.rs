//! CPU panel.

use super::theme::{self, *};
use super::widgets::*;
use crate::app::{App, Panel};
use crate::collect::Snapshot;
use crate::fmt;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

/// Preferred and minimum rows of the CPU history graph.
const GRAPH_WANT: usize = 8;
const GRAPH_MIN: usize = 5;
/// Narrowest core cell (meter and percentage, no clock).
const MIN_CELL: usize = 18;
/// Width at which the clock fits in a core cell.
const CLOCK_CELL: usize = 25;

pub fn draw(f: &mut Frame, area: Rect, app: &App, snap: &Snapshot) {
    let c = &snap.cpu;
    let sev = Sev::from_value(c.total, 70.0, 90.0);
    let title = format!("CPU · {} · {}C/{}T", c.model, c.physical_cores, c.threads);
    let block = panel_block(
        Panel::Cpu.num(),
        &title,
        Some((fmt::pct(c.total), theme::bold(sev.color()))),
        ACCENT_CPU,
        app.focus == Panel::Cpu,
        area.width,
    );
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width < 20 || inner.height == 0 {
        return;
    }

    let load_sev = Sev::from_value(c.load[0] / c.threads.max(1) as f64, 0.7, 1.0);
    let load = sev_value(
        format!("{:.2} · {:.2} · {:.2}", c.load[0], c.load[1], c.load[2]),
        load_sev,
    );
    let p = &snap.procs;
    let temp_row = snap.sensors.cpu_temp.map(|t| {
        (
            "Package temperature:".to_string(),
            sev_value(format!("{t:.0} °C"), Sev::from_value(t, 80.0, 90.0)),
        )
    });
    let power_row = (
        "Package power:".to_string(),
        match snap.sensors.cpu_power_w {
            Some(w) => vec![Span::styled(format!("{w:.1} W"), theme::text())],
            None => vec![Span::styled("needs root (or `amd_energy`)", theme::muted())],
        },
    );
    let mut rows = vec![
        (
            "CPU usage now:".to_string(),
            sev_value(fmt::pct(c.total), sev),
        ),
        ("Load average (1m, 5m, 15m):".to_string(), load),
        (
            "System uptime:".to_string(),
            vec![Span::styled(fmt::duration(c.uptime_secs), theme::text())],
        ),
        (
            "Processes:".to_string(),
            vec![Span::styled(
                format!(
                    "{} total · {} running · {} sleeping · {} threads",
                    p.total,
                    p.running,
                    p.sleeping,
                    fmt::thousands(p.threads as u64)
                ),
                theme::text(),
            )],
        ),
    ];
    rows.extend(temp_row);
    rows.push(power_row);
    let kv = kv_lines(rows);
    let mut notes: Vec<Line> = Vec::new();
    let boost = c.max_mhz.map(fmt::ghz).unwrap_or_else(|| "n/a".into());
    let avg = c
        .avg_freq_mhz()
        .map(fmt::ghz)
        .unwrap_or_else(|| "n/a".into());
    notes.push(Line::styled(
        format!(
            "Governor: {} · Max boost: {} · Average clock now: {}",
            c.governor.as_deref().unwrap_or("n/a"),
            boost,
            avg
        ),
        theme::muted(),
    ));
    if let (Some(lo), Some(hi), Some(av)) = (
        app.cpu_total.min(),
        app.cpu_total.max(),
        app.cpu_total.avg(),
    ) {
        notes.push(Line::styled(
            format!(
                "History: average {} · peak {} · low {}",
                fmt::pct(av),
                fmt::pct(hi),
                fmt::pct(lo)
            ),
            theme::muted(),
        ));
    }

    let n = c.cores.len();
    let mut avail = inner.height as usize;
    let mut y = inner.y;
    let take = |h: usize, avail: &mut usize, y: &mut u16| -> Option<Rect> {
        let h = h.min(*avail);
        if h == 0 {
            return None;
        }
        let r = Rect::new(inner.x, *y, inner.width, h as u16);
        *y += h as u16;
        *avail -= h;
        Some(r)
    };
    if let Some(r) = take(kv.len(), &mut avail, &mut y) {
        f.render_widget(Paragraph::new(kv), r);
    }
    if let Some(r) = take(notes.len(), &mut avail, &mut y) {
        f.render_widget(Paragraph::new(notes), r);
    }
    // The history graph matters more than per-core detail: use the fewest grid columns
    // (3-4 when wide) that still leave the graph its preferred height, and when even that
    // is not possible, squeeze the grid down to the graph's minimum.
    // A core cell needs about 24 columns with its clock, 17 without.
    let max_cols = (inner.width as usize / MIN_CELL).clamp(1, 4);
    let cols = (1..=max_cols)
        .find(|k| n.div_ceil(*k) + GRAPH_WANT <= avail)
        .unwrap_or(max_cols);
    let grid_rows = n.div_ceil(cols);
    let (grid_h, graph_h) = if avail > GRAPH_MIN {
        let g = grid_rows.min(avail - GRAPH_MIN);
        (g, avail - g)
    } else {
        (grid_rows.min(avail), 0)
    };
    if graph_h >= GRAPH_MIN
        && let Some(r) = take(graph_h, &mut avail, &mut y)
    {
        let data = app.cpu_total.view(app.span, app.interval_ms);
        let fmt_y = |v: f64| format!("{v:.0}%");
        let g = Graph {
            data: &data,
            max: 100.0,
            color: ACCENT_CPU,
            ylabel: &fmt_y,
            span_secs: app.span_secs(),
        };
        render_graph(f.buffer_mut(), r, &g);
    }
    if grid_h > 0
        && let Some(r) = take(grid_h, &mut avail, &mut y)
    {
        let cell_w = inner.width as usize / cols;
        let overhead = if cell_w >= CLOCK_CELL { 22 } else { 14 };
        let bar_w = cell_w.saturating_sub(overhead).clamp(4, 14);
        let lines: Vec<Line> = (0..grid_rows)
            .map(|row| {
                let mut spans: Vec<Span> = Vec::new();
                for col in 0..cols {
                    let i = row * cols + col;
                    let Some(core) = c.cores.get(i) else { break };
                    let sev = Sev::from_value(core.usage, 70.0, 90.0);
                    let mut cell = meter(
                        &format!("C{:02} ", i + 1),
                        core.usage / 100.0,
                        bar_w,
                        sev.color(),
                    );
                    let freq = core
                        .freq_mhz
                        .map(fmt::ghz)
                        .unwrap_or_else(|| "  n/a  ".into());
                    let mark = if sev == Sev::Ok { " " } else { sev.symbol() };
                    if cell_w >= CLOCK_CELL {
                        cell.push(Span::styled(format!(" {freq}"), theme::dim()));
                    }
                    cell.push(Span::styled(
                        format!(" {:>3.0}%{mark}", core.usage),
                        sev.style(),
                    ));
                    let w = spans_width(&cell);
                    spans.extend(cell);
                    spans.push(Span::raw(" ".repeat(cell_w.saturating_sub(w))));
                }
                Line::from(spans)
            })
            .collect();
        f.render_widget(Paragraph::new(lines), r);
    }
}
