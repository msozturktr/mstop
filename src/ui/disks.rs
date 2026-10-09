//! Disks panel: filesystems with fullness bars, per-disk throughput/latency, SMART details.

use super::theme::{self, *};
use super::widgets::*;
use crate::app::{App, Panel};
use crate::collect::Snapshot;
use crate::collect::disks::{DiskIo, Mount, SmartState};
use crate::fmt;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

pub const WARN_PCT: f64 = 85.0;
pub const CRIT_PCT: f64 = 95.0;
pub const SMART_HINT: &str = "SMART: run once as root or add a udev rule";

pub fn full_sev(m: &Mount) -> Sev {
    Sev::from_value(m.used_pct(), WARN_PCT, CRIT_PCT)
}

fn smart_badge(d: Option<&DiskIo>) -> Option<(String, Sev)> {
    match &d?.smart {
        SmartState::Ready(s) => match s.passed {
            Some(true) => Some(("✓ SMART OK".into(), Sev::Ok)),
            Some(false) => Some(("✖ SMART failing".into(), Sev::Crit)),
            None => None,
        },
        _ => None,
    }
}

fn mount_lines(m: &Mount, disks: &[DiskIo], w: usize) -> Vec<Line<'static>> {
    let disk = disks.iter().find(|d| d.name == m.disk);
    let sev = full_sev(m);
    let mid = if m.disk.is_empty() {
        format!("  {}", m.fstype)
    } else {
        format!("  {} · {}", m.disk, m.fstype)
    };
    let badge = smart_badge(disk);
    let badge_w = badge
        .as_ref()
        .map(|(t, _)| t.chars().count() + 2)
        .unwrap_or(0);
    let pct = m.used_pct();
    let usage = format!(
        " {} of {} used ({pct:.0}%)",
        fmt::bytes(m.used),
        fmt::bytes(m.total)
    );
    let bar_w = 10;
    let meter_w = bar_w + 2 + usage.chars().count();
    let one_line = w >= 36 + meter_w + mid.chars().count() + badge_w;
    let label_max = if one_line {
        18
    } else {
        w.saturating_sub(mid.chars().count() + badge_w).max(6)
    };
    let mut first = vec![
        Span::styled(truncate(&m.label(), label_max), theme::bold(TEXT)),
        Span::styled(mid, theme::dim()),
    ];
    let meter_spans = |first: &mut Vec<Span<'static>>| {
        first.extend(meter("", pct / 100.0, bar_w, sev.color()));
        first.push(Span::styled(usage.clone(), theme::bold(sev.color())));
    };
    if one_line {
        first.push(Span::raw("  "));
        meter_spans(&mut first);
    }
    if let Some((t, s)) = badge {
        first.push(Span::raw("  "));
        first.push(Span::styled(t, s.style()));
    }
    let mut out = vec![Line::from(first)];
    if !one_line {
        let mut second = vec![Span::raw("  ")];
        meter_spans(&mut second);
        out.push(Line::from(second));
    }
    out
}

fn lat(v: Option<f64>) -> String {
    v.map(|v| format!("{v:.1} ms"))
        .unwrap_or_else(|| "- ms".into())
}

fn disk_lines(d: &DiskIo, w: usize) -> Vec<Line<'static>> {
    let model = d.model.as_deref().unwrap_or("unknown model");
    let mut out = vec![Line::from(vec![
        Span::styled(d.name.clone(), theme::bold(ACCENT_DISK)),
        Span::styled(
            truncate(
                &format!(" · {model} · {} {}", fmt::bytes(d.size), d.kind()),
                w.saturating_sub(d.name.chars().count()),
            ),
            theme::dim(),
        ),
    ])];
    let io = format!(
        "R {} ({} IOPS, {}) · W {} ({} IOPS, {})",
        fmt::rate(d.read_bps),
        fmt::thousands(d.read_iops.round() as u64),
        lat(d.read_lat_ms),
        fmt::rate(d.write_bps),
        fmt::thousands(d.write_iops.round() as u64),
        lat(d.write_lat_ms),
    );
    out.push(Line::styled(
        format!("  {}", truncate(&io, w.saturating_sub(2))),
        theme::text(),
    ));
    let sev = Sev::from_value(d.busy_pct, 70.0, 90.0);
    let mut third = vec![
        Span::raw("  "),
        Span::styled(format!("busy {:.0}%", d.busy_pct), theme::bold(sev.color())),
    ];
    let mut extra = String::new();
    if let SmartState::Ready(s) = &d.smart {
        if let Some(t) = s.temp_c {
            extra.push_str(&format!(" · {t:.0} °C"));
        }
        if let Some(h) = s.power_on_hours {
            extra.push_str(&format!(" · {} h powered on", fmt::thousands(h)));
        }
        if let Some(p) = s.pct_used {
            extra.push_str(&format!(" · {p:.0}% worn"));
        }
    }
    third.push(Span::styled(
        truncate(&extra, w.saturating_sub(12)),
        theme::dim(),
    ));
    out.push(Line::from(third));
    if let SmartState::Ready(s) = &d.smart
        && s.media_errors.unwrap_or(0) > 0
    {
        out.push(Line::styled(
            format!("  {} media errors", s.media_errors.unwrap_or(0)),
            theme::bold(WARN),
        ));
    }
    if d.smart == SmartState::NeedsRoot {
        out.push(Line::styled(format!("  {SMART_HINT}"), theme::muted()));
    }
    out
}

pub fn lines(snap: &Snapshot, w: usize) -> Vec<Line<'static>> {
    let di = &snap.disks;
    let mut out = Vec::new();
    for m in &di.mounts {
        out.extend(mount_lines(m, &di.disks, w));
    }
    for d in &di.disks {
        if !out.is_empty() {
            out.push(Line::raw(""));
        }
        out.extend(disk_lines(d, w));
    }
    if out.is_empty() {
        out.push(Line::styled("No disks detected.", theme::muted()));
    }
    out
}

pub fn draw(f: &mut Frame, area: Rect, app: &App, snap: &Snapshot) {
    let di = &snap.disks;
    let headline = Some((
        format!(
            "R {} · W {}",
            fmt::rate(di.read_bps()),
            fmt::rate(di.write_bps())
        ),
        theme::bold(ACCENT_DISK),
    ));
    let block = panel_block(
        Panel::Disks.num(),
        "Disks",
        headline,
        ACCENT_DISK,
        app.focus == Panel::Disks,
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
    if rest >= 7 && !di.disks.is_empty() {
        f.render_widget(
            Paragraph::new(Line::styled("Disk read + write history", theme::dim())),
            Rect::new(inner.x, inner.y + used + 1, inner.width, 1),
        );
        let data = app.disk_io.view(app.span, app.interval_ms);
        let peak = data.iter().cloned().fold(0.0, f64::max);
        let fmt_y = |v: f64| fmt::rate(v);
        let gr = Graph {
            data: &data,
            max: nice_max(peak, 1_000_000.0),
            color: ACCENT_DISK,
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
