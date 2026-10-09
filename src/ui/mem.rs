//! Memory panel.

use super::theme::{self, *};
use super::widgets::*;
use crate::app::{App, Panel};
use crate::collect::Snapshot;
use crate::fmt;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

fn pct_of(a: u64, b: u64) -> f64 {
    if b == 0 {
        0.0
    } else {
        a as f64 / b as f64 * 100.0
    }
}

pub fn draw(f: &mut Frame, area: Rect, app: &App, snap: &Snapshot) {
    let m = &snap.mem;
    let used_pct = pct_of(m.used, m.total);
    let sev = Sev::from_value(used_pct, 80.0, 92.0);
    let title = format!("Memory · {} total", fmt::bytes(m.total));
    let block = panel_block(
        2,
        &title,
        Some((fmt::pct(used_pct), theme::bold(sev.color()))),
        ACCENT_MEM,
        app.focus == Panel::Mem,
        area.width,
    );
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width < 20 || inner.height == 0 {
        return;
    }
    let plain = |s: String| vec![Span::styled(s, theme::text())];
    let mut items = vec![
        (
            "Memory used currently:".to_string(),
            sev_value(
                format!(
                    "{} of {} ({})",
                    fmt::bytes(m.used),
                    fmt::bytes(m.total),
                    fmt::pct(used_pct)
                ),
                sev,
            ),
        ),
        (
            "Available for programs:".to_string(),
            plain(fmt::bytes(m.available)),
        ),
        (
            "Cache + buffers (reclaimable):".to_string(),
            plain(fmt::bytes(m.cache)),
        ),
        (
            "Shared memory (tmpfs):".to_string(),
            plain(fmt::bytes(m.shared)),
        ),
        (
            "Dirty (waiting to be written):".to_string(),
            plain(fmt::bytes(m.dirty)),
        ),
        ("Kernel slab:".to_string(), plain(fmt::bytes(m.slab))),
    ];
    for z in &m.zram {
        let text = match (z.orig, z.compr, z.ratio(), z.saved()) {
            (Some(o), Some(c), Some(r), Some(s)) => {
                format!(
                    "{} stored in {} ({r:.1}:1), saved {}",
                    fmt::bytes(o),
                    fmt::bytes(c),
                    fmt::bytes(s)
                )
            }
            (Some(o), Some(c), _, _) => format!("{} stored in {}", fmt::bytes(o), fmt::bytes(c)),
            _ => z
                .disksize
                .map(|d| format!("{} device, no stats", fmt::bytes(d)))
                .unwrap_or_else(|| "no stats".into()),
        };
        items.push((format!("ZRAM ({}):", z.name), plain(text)));
    }
    if m.swap_total > 0 {
        let sp = pct_of(m.swap_used, m.swap_total);
        let ssev = Sev::from_value(sp, 25.0, 60.0);
        items.push((
            "Swap used:".to_string(),
            sev_value(
                format!(
                    "{} of {} ({})",
                    fmt::bytes(m.swap_used),
                    fmt::bytes(m.swap_total),
                    fmt::pct(sp)
                ),
                ssev,
            ),
        ));
    } else {
        items.push(("Swap used:".to_string(), plain("no swap configured".into())));
    }
    let mut lines = kv_lines(items);

    // Stacked RAM bar + legend, placed after the six basic rows.
    let w = inner.width as usize;
    let total = m.total.max(1) as f64;
    let used_w = ((m.used as f64 / total) * w as f64).round() as usize;
    let cache_w =
        (((m.cache as f64 / total) * w as f64).round() as usize).min(w.saturating_sub(used_w));
    let free_w = w.saturating_sub(used_w + cache_w);
    let stacked = Line::from(vec![
        Span::styled("━".repeat(used_w), Style::default().fg(ACCENT_MEM)),
        Span::styled("━".repeat(cache_w), Style::default().fg(CACHE)),
        Span::styled("━".repeat(free_w), Style::default().fg(TRACK)),
    ]);
    let legend = Line::from(vec![
        Span::styled("■ ", Style::default().fg(ACCENT_MEM)),
        Span::styled(format!("used {}   ", fmt::bytes(m.used)), theme::dim()),
        Span::styled("■ ", Style::default().fg(CACHE)),
        Span::styled(
            format!("cache+buffers {}   ", fmt::bytes(m.cache)),
            theme::dim(),
        ),
        Span::styled("■ ", Style::default().fg(TRACK)),
        Span::styled(format!("free {}", fmt::bytes(m.free())), theme::dim()),
    ]);
    let at = 6.min(lines.len());
    lines.insert(at, legend);
    lines.insert(at, stacked);

    let h = (lines.len() as u16).min(inner.height);
    f.render_widget(
        Paragraph::new(lines),
        Rect::new(inner.x, inner.y, inner.width, h),
    );

    let rest = inner.height - h;
    if rest >= 5 {
        let r = Rect::new(inner.x, inner.y + h, inner.width, rest);
        let data: Vec<f64> = app.ram.view(app.span, app.interval_ms);
        let total_b = m.total as f64;
        let fmt_y = |v: f64| fmt::bytes(v as u64);
        let g = Graph {
            data: &data,
            max: total_b,
            color: ACCENT_MEM,
            ylabel: &fmt_y,
            span_secs: app.span_secs(),
        };
        render_graph(f.buffer_mut(), r, &g);
    }
}
