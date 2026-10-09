//! Shared drawing helpers.

use super::theme::{self, *};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Padding};

pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else if max == 0 {
        String::new()
    } else {
        let mut t: String = s.chars().take(max - 1).collect();
        t.push('…');
        t
    }
}

/// Panel block with rounded borders and the title fully inside the top border.
pub fn panel_block(
    num: u8,
    title: &str,
    headline: Option<(String, Style)>,
    accent: Color,
    focused: bool,
    width: u16,
) -> Block<'static> {
    let border = Style::default().fg(if focused { accent } else { BORDER });
    let hl_len = headline
        .as_ref()
        .map(|(h, _)| h.chars().count() + 4)
        .unwrap_or(0);
    let avail = (width as usize).saturating_sub(2);
    let left_max = avail.saturating_sub(hl_len + 6);
    let title = truncate(title, left_max);
    let left = Line::from(vec![
        Span::styled("─ ", border),
        Span::styled(format!("{num} "), theme::bold(accent)),
        Span::styled(title, theme::bold(TEXT)),
        Span::raw(" "),
    ]);
    let mut block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(border)
        .style(Style::default().bg(PANEL_BG).fg(TEXT))
        .padding(Padding::horizontal(1))
        .title(left);
    if let Some((h, st)) = headline {
        block = block.title(
            Line::from(vec![
                Span::raw(" "),
                Span::styled(h, st),
                Span::styled(" ─", border),
            ])
            .right_aligned(),
        );
    }
    block
}

/// Horizontal meter drawn as a heavy line: filled cells in `fg`, a half cell (`╸`) for the
/// remainder, and the rest of the track in `TRACK`. Line glyphs stay inside their cell in
/// every terminal, unlike block elements, which some terminals stretch to the line height.
pub fn bar(frac: f64, width: usize, fg: Color) -> Vec<Span<'static>> {
    let cells = frac.clamp(0.0, 1.0) * width as f64;
    let full = (cells.floor() as usize).min(width);
    let mut filled = "━".repeat(full);
    let mut used = full;
    if full < width && cells - full as f64 >= 0.5 {
        filled.push('╸');
        used += 1;
    }
    vec![
        Span::styled(filled, Style::default().fg(fg)),
        Span::styled("━".repeat(width - used), Style::default().fg(TRACK)),
    ]
}

/// Labeled meter: `label ━━━━━━━━ ` (one space of padding on each side of the bar).
pub fn meter(label: &str, frac: f64, bar_w: usize, fg: Color) -> Vec<Span<'static>> {
    let mut v = vec![
        Span::styled(label.to_string(), theme::dim()),
        Span::raw(" "),
    ];
    v.extend(bar(frac, bar_w, fg));
    v.push(Span::raw(" "));
    v
}

pub fn spans_width(spans: &[Span]) -> usize {
    spans.iter().map(|s| s.content.chars().count()).sum()
}

/// Label-left / value-right rows. Labels are padded to a common width so values line up
/// in one column.
pub fn kv_lines(items: Vec<(String, Vec<Span<'static>>)>) -> Vec<Line<'static>> {
    kv_lines_gap(items, 3)
}

/// `kv_lines` with a custom minimum gap between the longest label and the values.
pub fn kv_lines_gap(items: Vec<(String, Vec<Span<'static>>)>, gap: usize) -> Vec<Line<'static>> {
    let lw = items
        .iter()
        .map(|(l, _)| l.chars().count())
        .max()
        .unwrap_or(0)
        + gap;
    items
        .into_iter()
        .map(|(label, value)| {
            let gap = lw - label.chars().count();
            let mut spans = vec![
                Span::styled(label, theme::dim()),
                Span::raw(" ".repeat(gap)),
            ];
            spans.extend(value);
            Line::from(spans)
        })
        .collect()
}

/// Value followed by a severity symbol, coloured by severity.
pub fn sev_value(text: String, sev: Sev) -> Vec<Span<'static>> {
    vec![
        Span::styled(text, theme::bold(sev.color())),
        Span::styled(format!(" {}", sev.symbol()), sev.style()),
    ]
}

pub struct Graph<'a> {
    /// Samples, oldest first.
    pub data: &'a [f64],
    pub max: f64,
    pub color: Color,
    pub ylabel: &'a dyn Fn(f64) -> String,
    pub span_secs: u64,
}

/// Number of history samples visible in a graph.
pub const WINDOW: usize = 60;

fn time_label(secs: u64) -> String {
    if secs >= 3600 && secs.is_multiple_of(3600) {
        format!("{}h", secs / 3600)
    } else if secs >= 120 && secs.is_multiple_of(60) {
        format!("{}m", secs / 60)
    } else if secs >= 120 {
        format!("{:.1}m", secs as f64 / 60.0)
    } else {
        format!("{secs}s")
    }
}

fn put(buf: &mut Buffer, x: u16, y: u16, s: &str, style: Style, max_x: u16) {
    for (i, ch) in s.chars().enumerate() {
        let cx = x + i as u16;
        if cx >= max_x {
            break;
        }
        buf[(cx, y)].set_char(ch).set_style(style);
    }
}

/// One line of a multi-series graph.
pub struct Series<'a> {
    pub data: &'a [f64],
    pub color: Color,
}

/// Smallest 1/2/5 x 10^n value that is at least `v` (and at least `floor`): a tidy axis top.
pub fn nice_max(v: f64, floor: f64) -> f64 {
    let v = v.max(floor);
    let mag = 10f64.powf(v.log10().floor());
    for m in [1.0, 2.0, 5.0, 10.0] {
        if v <= m * mag * 1.0000001 {
            return m * mag;
        }
    }
    10.0 * mag
}

/// Braille area graph with y-axis labels and a time axis.
pub fn render_graph(buf: &mut Buffer, area: Rect, g: &Graph) {
    let s = [Series {
        data: g.data,
        color: g.color,
    }];
    render_series(buf, area, &s, g.max, g.ylabel, g.span_secs);
}

/// Like `render_graph` for several series sharing one axis; later series are drawn on top
/// (their colour wins where cells overlap).
pub fn render_series(
    buf: &mut Buffer,
    area: Rect,
    series: &[Series],
    max: f64,
    ylabel: &dyn Fn(f64) -> String,
    span_secs: u64,
) {
    if area.height < 4 || area.width < 14 {
        return;
    }
    let fracs: Vec<f64> = if area.height > 9 {
        vec![0.0, 0.25, 0.5, 0.75, 1.0]
    } else if area.height > 5 {
        vec![0.0, 0.5, 1.0]
    } else {
        vec![0.0, 1.0]
    };
    let labels: Vec<String> = fracs.iter().map(|f| ylabel(f * max)).collect();
    let lw = labels.iter().map(|l| l.chars().count()).max().unwrap_or(0) as u16 + 1;
    let plot_w = area.width.saturating_sub(lw) as usize;
    let plot_h = (area.height - 1) as usize;
    if plot_w < 6 {
        return;
    }
    let px = area.x + lw;
    let dim = Style::default().fg(DIM).bg(PANEL_BG);
    for (f, l) in fracs.iter().zip(&labels) {
        let row = ((1.0 - f) * (plot_h - 1) as f64).round() as u16;
        let x = area.x + lw - 1 - l.chars().count() as u16;
        put(buf, x, area.y + row, l, dim, area.x + area.width);
    }
    // Rasterise into braille cells.
    let mut cells = vec![0u8; plot_w * plot_h];
    let mut owner = vec![0usize; plot_w * plot_h];
    let dots_w = plot_w * 2;
    const LEFT: [u8; 4] = [0x01, 0x02, 0x04, 0x40];
    const RIGHT: [u8; 4] = [0x08, 0x10, 0x20, 0x80];
    for (si, s) in series.iter().enumerate() {
        let n = s.data.len();
        if n == 0 || max <= 0.0 {
            continue;
        }
        for x in 0..dots_w {
            let t = x as f64 / (dots_w - 1) as f64 * (WINDOW - 1) as f64
                - (WINDOW.saturating_sub(n)) as f64;
            if t < -0.001 {
                continue;
            }
            let t = t.clamp(0.0, (n - 1) as f64);
            let (i0, i1) = (t.floor() as usize, (t.ceil() as usize).min(n - 1));
            let v = s.data[i0] + (s.data[i1] - s.data[i0]) * (t - i0 as f64);
            let mut h = ((v / max).clamp(0.0, 1.0) * (plot_h * 4) as f64).round() as usize;
            if h == 0 && v > 0.0 {
                h = 1;
            }
            let cx = x / 2;
            let table = if x % 2 == 0 { &LEFT } else { &RIGHT };
            for yb in 0..h {
                let cy = plot_h - 1 - yb / 4;
                cells[cy * plot_w + cx] |= table[3 - yb % 4];
                owner[cy * plot_w + cx] = si;
            }
        }
    }
    for cy in 0..plot_h {
        for cx in 0..plot_w {
            let bits = cells[cy * plot_w + cx];
            if bits != 0 {
                let ch = char::from_u32(0x2800 + bits as u32).unwrap_or(' ');
                let color = series[owner[cy * plot_w + cx]].color;
                buf[(px + cx as u16, area.y + cy as u16)]
                    .set_char(ch)
                    .set_style(Style::default().fg(color).bg(PANEL_BG));
            }
        }
    }
    // Time axis.
    let ty = area.y + plot_h as u16;
    let marks = [
        (0.0, format!("{} ago", time_label(span_secs))),
        (0.25, time_label(span_secs * 3 / 4)),
        (0.5, time_label(span_secs / 2)),
        (0.75, time_label(span_secs / 4)),
        (1.0, "now".to_string()),
    ];
    let mut next_free = 0usize;
    for (frac, text) in marks {
        let len = text.chars().count();
        let start = if frac == 0.0 {
            0
        } else if frac == 1.0 {
            plot_w.saturating_sub(len)
        } else {
            ((plot_w as f64 * frac) as usize).saturating_sub(len / 2)
        };
        if start < next_free || start + len > plot_w {
            continue;
        }
        put(
            buf,
            px + start as u16,
            ty,
            &text,
            Style::default().fg(MUTED).bg(PANEL_BG),
            area.x + area.width,
        );
        next_free = start + len + 2;
    }
}
