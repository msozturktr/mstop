//! Process table panel.

use super::theme::{self, *};
use super::widgets::*;
use crate::app::{App, Panel};
use crate::collect::{Snapshot, procs};
use crate::fmt;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

fn cell(s: &str, w: usize, right: bool) -> String {
    let t = truncate(s, w);
    if right {
        format!("{t:>w$}")
    } else {
        format!("{t:<w$}")
    }
}

pub fn draw(f: &mut Frame, area: Rect, app: &App, snap: &Snapshot) {
    let arrow = if app.sort_desc_effective() {
        "▼"
    } else {
        "▲"
    };
    let headline = format!(
        "sort: {} {}{}",
        app.sort.label(),
        arrow,
        if app.tree { " · tree" } else { "" }
    );
    let title = format!("Processes · {} total", snap.procs.total);
    let block = panel_block(
        Panel::Procs.num(),
        &title,
        Some((headline, theme::bold(ACCENT_PROCS))),
        ACCENT_PROCS,
        app.focus == Panel::Procs,
        area.width,
    );
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width < 30 || inner.height < 3 {
        return;
    }

    // Filter line.
    let total = snap.procs.total;
    let mut fl = vec![Span::styled("/ filter: ", theme::bold(ACCENT_PROCS))];
    if app.filter.is_empty() && !app.filter_mode {
        fl.push(Span::styled(
            "(press / to search by name, command, user or PID)",
            theme::muted(),
        ));
    } else {
        fl.push(Span::styled(app.filter.clone(), theme::text()));
        if app.filter_mode {
            fl.push(Span::styled("▌", theme::bold(ACCENT_PROCS)));
        }
        fl.push(Span::styled(
            format!("   [{} matches shown of {}]", app.matches, total),
            theme::dim(),
        ));
    }
    f.render_widget(
        Paragraph::new(Line::from(fl)),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );

    // Column layout.
    let w = inner.width as usize;
    let wide = w >= 90;
    let gpu_cols = w >= 80;
    let (w_pid, w_user, w_cpu, w_mem, w_io, w_thr, w_state) = (
        7,
        if wide { 12 } else { 0 },
        7,
        10,
        11,
        if wide { 4 } else { 0 },
        5,
    );
    let (w_gpu, w_vram) = if gpu_cols { (6, 10) } else { (0, 0) };
    let gaps = 6 + usize::from(wide) * 2 + usize::from(gpu_cols) * 2;
    let fixed = w_pid + w_user + w_cpu + w_mem + w_gpu + w_vram + w_io + w_thr + w_state + gaps;
    let w_name = w.saturating_sub(fixed).max(8);

    let head_style = Style::default().fg(DIM).add_modifier(Modifier::BOLD);
    let mut head = format!("{} ", cell("PID", w_pid, true));
    if wide {
        head += &format!("{} ", cell("USER", w_user, false));
    }
    head += &format!(
        "{} {} {} ",
        cell("NAME/COMMAND", w_name, false),
        cell("CPU%", w_cpu, true),
        cell("MEM", w_mem, true),
    );
    if gpu_cols {
        head += &format!(
            "{} {} ",
            cell("GPU%", w_gpu, true),
            cell("VRAM", w_vram, true)
        );
    }
    head += &format!("{} ", cell("DISK I/O", w_io, true));
    if wide {
        head += &format!("{} ", cell("THR", w_thr, true));
    }
    head += &cell("STATE", w_state, false);
    f.render_widget(
        Paragraph::new(Line::styled(head, head_style)),
        Rect::new(inner.x, inner.y + 1, inner.width, 1),
    );

    // Rows.
    let detail_row = inner.height >= 6;
    let view_h = (inner.height - 2 - u16::from(detail_row)) as usize;
    app.view_h.set(view_h);
    let mut off = app.scroll.get();
    if app.sel < off {
        off = app.sel;
    }
    if app.sel >= off + view_h {
        off = app.sel + 1 - view_h;
    }
    off = off.min(app.rows.len().saturating_sub(view_h));
    app.scroll.set(off);

    let mut lines: Vec<Line> = Vec::new();
    for (i, row) in app.rows.iter().enumerate().skip(off).take(view_h) {
        let p = &snap.procs.list[row.idx];
        let selected = i == app.sel;
        let base = if selected {
            Style::default().bg(SELECTION)
        } else {
            Style::default()
        };
        let cpu_sev = Sev::from_value(p.cpu, 50.0, 90.0);
        let state_color = match p.state {
            'R' => OK,
            'D' => WARN,
            'Z' => CRIT,
            _ => DIM,
        };
        let mut spans: Vec<Span> = vec![Span::styled(
            format!("{} ", cell(&p.pid.to_string(), w_pid, true)),
            theme::dim(),
        )];
        if wide {
            spans.push(Span::styled(
                format!("{} ", cell(&p.user, w_user, false)),
                theme::dim(),
            ));
        }
        // Name + dim arguments, tree prefix first.
        let prefix_w = row.prefix.chars().count();
        let name = truncate(&p.name, w_name.saturating_sub(prefix_w));
        let name_w = prefix_w + name.chars().count();
        let args = p.cmd.split_once(' ').map(|x| x.1).unwrap_or("");
        let room = w_name.saturating_sub(name_w + 1);
        let args = if args.is_empty() || room < 4 {
            String::new()
        } else {
            format!(" {}", truncate(args, room))
        };
        let pad = w_name.saturating_sub(name_w + args.chars().count());
        spans.push(Span::styled(row.prefix.clone(), theme::muted()));
        spans.push(Span::styled(name, theme::bold(TEXT)));
        spans.push(Span::styled(args, theme::muted()));
        spans.push(Span::raw(" ".repeat(pad + 1)));
        spans.push(Span::styled(
            format!("{} ", cell(&format!("{:.1}", p.cpu), w_cpu, true)),
            if cpu_sev == Sev::Ok {
                theme::text()
            } else {
                cpu_sev.style()
            },
        ));
        spans.push(Span::styled(
            format!("{} ", cell(&fmt::bytes(p.mem), w_mem, true)),
            theme::text(),
        ));
        if gpu_cols {
            let (g, v) = match (p.gpu_pct, p.gpu_vram) {
                (None, None) => ("-".to_string(), "-".to_string()),
                (g, v) => (
                    g.map(|x| format!("{x:.0}")).unwrap_or_else(|| "-".into()),
                    v.filter(|v| *v > 0)
                        .map(fmt::bytes)
                        .unwrap_or_else(|| "-".into()),
                ),
            };
            let st = if p.gpu_pct.unwrap_or(0.0) > 0.0 || p.gpu_vram.unwrap_or(0) > 0 {
                theme::text()
            } else {
                theme::muted()
            };
            spans.push(Span::styled(format!("{} ", cell(&g, w_gpu, true)), st));
            spans.push(Span::styled(format!("{} ", cell(&v, w_vram, true)), st));
        }
        let io = p.io_bps();
        spans.push(Span::styled(
            format!("{} ", cell(&fmt::rate(io), w_io, true)),
            if io > 0.5 {
                theme::text()
            } else {
                theme::muted()
            },
        ));
        if wide {
            spans.push(Span::styled(
                format!("{} ", cell(&p.threads.to_string(), w_thr, true)),
                theme::dim(),
            ));
        }
        spans.push(Span::styled(
            cell(&p.state.to_string(), w_state, false),
            Style::default().fg(state_color),
        ));
        let mut line = Line::from(spans);
        if selected {
            let used: usize = line.spans.iter().map(|s| s.content.chars().count()).sum();
            line.spans
                .push(Span::raw(" ".repeat(w.saturating_sub(used))));
        }
        lines.push(line.style(base));
    }
    if lines.is_empty() {
        lines.push(Line::styled(
            "No processes match the filter.",
            theme::muted(),
        ));
    }
    f.render_widget(
        Paragraph::new(lines),
        Rect::new(inner.x, inner.y + 2, inner.width, view_h as u16),
    );
    if detail_row && let Some(p) = app.selected() {
        let d = procs::detail(p.pid);
        let na = || "n/a".to_string();
        let text = format!(
            "Selected PID {}: {} threads · {} open files · RSS {} · VMS {} · nice {} · cgroup {}",
            p.pid,
            p.threads,
            d.open_files.map(|n| n.to_string()).unwrap_or_else(na),
            fmt::bytes(p.mem),
            d.vms.map(fmt::bytes).unwrap_or_else(na),
            d.nice.map(|n| n.to_string()).unwrap_or_else(na),
            d.cgroup.as_deref().unwrap_or("n/a"),
        );
        f.render_widget(
            Paragraph::new(Line::styled(truncate(&text, w), theme::dim())),
            Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1),
        );
    }
}
