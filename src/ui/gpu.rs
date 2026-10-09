//! GPU panel: one block per GPU (discrete first), utilisation history, apps using the GPU.

use super::theme::{self, *};
use super::widgets::*;
use crate::app::{App, Panel};
use crate::collect::Snapshot;
use crate::collect::gpu::Gpu;
use crate::fmt;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

/// GPUs in display order: discrete first, then by index.
pub fn ordered(snap: &Snapshot) -> Vec<&Gpu> {
    let mut v: Vec<&Gpu> = snap.gpus.iter().collect();
    v.sort_by_key(|g| (!g.discrete, g.index));
    v
}

/// The active GPU shown in the history graph: busiest awake GPU (discrete wins ties).
fn graph_gpu<'a>(gpus: &[&'a Gpu]) -> Option<&'a Gpu> {
    let mut best: Option<&Gpu> = None;
    for g in gpus.iter().copied().filter(|g| !g.sleeping) {
        if best.is_none_or(|b| g.util.unwrap_or(0.0) > b.util.unwrap_or(0.0)) {
            best = Some(g);
        }
    }
    best
}

fn plain(s: String) -> Vec<Span<'static>> {
    vec![Span::styled(s, theme::text())]
}

fn gpu_block(g: &Gpu, w: usize) -> Vec<Line<'static>> {
    let kind = if g.discrete { "discrete" } else { "integrated" };
    let mut out = vec![Line::styled(
        truncate(&format!("GPU {} · {} · {kind}", g.index, g.name), w),
        theme::bold(ACCENT_GPU),
    )];
    if g.sleeping {
        out.push(Line::styled(
            truncate("● Sleeping (power saving) — not woken to save battery", w),
            theme::dim(),
        ));
        return out;
    }
    let bar_w = w.saturating_sub(21 + 24).clamp(6, 24);
    let mut items: Vec<(String, Vec<Span<'static>>)> = Vec::new();
    if let Some(u) = g.util {
        let sev = Sev::from_value(u, 70.0, 90.0);
        let mut v = bar(u / 100.0, bar_w, sev.color());
        v.push(Span::styled(format!(" {u:.0}%"), sev.style()));
        items.push(("Usage:".into(), v));
    }
    if let (Some(used), Some(total)) = (g.vram_used, g.vram_total)
        && total > 0
    {
        let frac = used as f64 / total as f64;
        let sev = Sev::from_value(frac * 100.0, 80.0, 92.0);
        let mut v = bar(frac, bar_w, sev.color());
        v.push(Span::styled(
            format!(" {} of {}", fmt::bytes(used), fmt::bytes(total)),
            theme::text(),
        ));
        items.push(("Video memory used:".into(), v));
    }
    if !g.discrete
        && let (Some(used), Some(total)) = (g.gtt_used, g.gtt_total)
        && total > 0
    {
        items.push((
            "Shared memory (GTT):".into(),
            plain(format!("{} of {}", fmt::bytes(used), fmt::bytes(total))),
        ));
    }
    if let Some(t) = g.temp_c {
        items.push((
            "Temperature:".into(),
            sev_value(format!("{t:.0} °C"), Sev::from_value(t, 80.0, 90.0)),
        ));
    }
    if let Some(p) = g.power_w {
        let txt = match g.power_cap_w.filter(|c| *c > 0.0) {
            Some(c) => format!("{p:.1} W of {c:.0} W limit"),
            None => format!("{p:.1} W"),
        };
        items.push(("Power draw:".into(), plain(txt)));
    }
    if g.clock_mhz.is_some() || g.mem_clock_mhz.is_some() {
        let mhz = |c: Option<u32>| {
            c.map(|c| format!("{c} MHz"))
                .unwrap_or_else(|| "n/a".into())
        };
        let mut txt = format!(
            "{} graphics · {} memory",
            mhz(g.clock_mhz),
            mhz(g.mem_clock_mhz)
        );
        if let Some(ps) = &g.pstate {
            txt.push_str(&format!(" · {ps}"));
        }
        items.push(("Clock:".into(), plain(truncate(&txt, w.saturating_sub(21)))));
    }
    let fan = match (g.fan_pct, g.fan_rpm) {
        (Some(p), _) => plain(format!("{p:.0}%")),
        (None, Some(r)) => plain(format!("{r} RPM")),
        _ => vec![Span::styled("not reported", theme::muted())],
    };
    items.push(("Fan:".into(), fan));
    out.extend(kv_lines(items));
    out.push(Line::styled("Apps using this GPU:", theme::dim()));
    let apps: Vec<_> = g.procs.iter().take(5).collect();
    if apps.is_empty() {
        out.push(Line::styled("  (none)", theme::muted()));
    }
    let nw = w.saturating_sub(2 + 1 + 6 + 1 + 10).clamp(8, 24);
    for p in apps {
        let pct = p
            .gpu_pct
            .map(|v| format!("{v:.0}%"))
            .unwrap_or_else(|| "-".into());
        let vram = p.vram.map(fmt::bytes).unwrap_or_else(|| "-".into());
        out.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("{:<nw$}", truncate(&p.name, nw)), theme::text()),
            Span::styled(format!(" {pct:>6}"), theme::text()),
            Span::styled(format!(" {vram:>10}"), theme::dim()),
        ]));
    }
    out
}

pub fn lines(snap: &Snapshot, w: usize) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for g in ordered(snap) {
        if !out.is_empty() {
            out.push(Line::raw(""));
        }
        out.extend(gpu_block(g, w));
    }
    if out.is_empty() {
        out.push(Line::styled("No GPU detected.", theme::muted()));
    }
    out
}

pub fn draw(f: &mut Frame, area: Rect, app: &App, snap: &Snapshot) {
    let gpus = ordered(snap);
    let primary = graph_gpu(&gpus);
    let headline = match (primary, gpus.is_empty()) {
        (Some(g), _) => {
            let sev = Sev::from_value(g.util.unwrap_or(0.0), 70.0, 90.0);
            Some((
                g.util.map(fmt::pct).unwrap_or_else(|| "active".into()),
                theme::bold(sev.color()),
            ))
        }
        (None, false) => Some(("asleep".to_string(), theme::dim())),
        _ => None,
    };
    let block = panel_block(
        Panel::Gpu.num(),
        &format!(
            "GPU · {} device{}",
            gpus.len(),
            if gpus.len() == 1 { "" } else { "s" }
        ),
        headline,
        ACCENT_GPU,
        app.focus == Panel::Gpu,
        area.width,
    );
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width < 20 || inner.height == 0 {
        return;
    }
    let ls = lines(snap, inner.width as usize);
    let used = (ls.len() as u16).min(inner.height);
    f.render_widget(
        Paragraph::new(ls),
        Rect::new(inner.x, inner.y, inner.width, used),
    );
    let rest = inner.height - used;
    if let (Some(g), true) = (primary, rest >= 7)
        && let Some(h) = app.gpu_util.get(g.index)
    {
        f.render_widget(
            Paragraph::new(Line::styled(
                format!("GPU {} utilization history", g.index),
                theme::dim(),
            )),
            Rect::new(inner.x, inner.y + used + 1, inner.width, 1),
        );
        let data = h.view(app.span, app.interval_ms);
        let fmt_y = |v: f64| format!("{v:.0}%");
        let gr = Graph {
            data: &data,
            max: 100.0,
            color: ACCENT_GPU,
            ylabel: &fmt_y,
            span_secs: app.span_secs(),
        };
        render_graph(
            f.buffer_mut(),
            Rect::new(inner.x, inner.y + used + 2, inner.width, rest - 2),
            &gr,
        );
    }
}
