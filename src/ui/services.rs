//! Services & Alerts panel: systemd health, containers and the recent alert log.

use super::theme::{self, *};
use super::widgets::*;
use crate::alerts::{Alert, Kind, Severity, local_hm};
use crate::app::{App, Panel};
use crate::collect::Snapshot;
use crate::collect::services::{
    ContainersState, NO_ACCESS_HINT, Systemd, SystemdState, next_timer_text,
};
use crate::fmt;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

/// Most failed-unit lines shown before collapsing into "+N more".
const MAX_FAILED_LINES: usize = 4;
/// Container detail lines shown when the panel is tall.
const MAX_CONTAINER_LINES: usize = 3;

pub fn failed_count(snap: &Snapshot) -> usize {
    match &snap.services.systemd {
        SystemdState::Ready(s) => s.failed.len(),
        _ => 0,
    }
}

/// One line, or two when the next-timer note does not fit.
pub fn summary_lines(state: &SystemdState, w: usize) -> Vec<Line<'static>> {
    match state {
        SystemdState::Pending => vec![Line::styled("Services: checking…", theme::muted())],
        SystemdState::Unavailable => {
            vec![Line::styled(
                "Services: systemd is not available",
                theme::muted(),
            )]
        }
        SystemdState::Ready(s) => {
            let fsev = if s.failed.is_empty() {
                Sev::Ok
            } else {
                Sev::Crit
            };
            let sep = || Span::styled(" · ", theme::muted());
            let mut v = vec![
                Span::styled("Services: ", theme::dim()),
                Span::styled(format!("{} running", s.running), theme::text()),
                sep(),
                Span::styled(
                    format!("{} failed", s.failed.len()),
                    theme::bold(fsev.color()),
                ),
                sep(),
                Span::styled(
                    format!("{} timer{}", s.timers, if s.timers == 1 { "" } else { "s" }),
                    theme::text(),
                ),
            ];
            let mut lines = vec![];
            if s.next_timer.is_some() {
                let next = format!(" (next: {})", next_timer_text(s));
                if spans_width(&v) + next.chars().count() <= w {
                    v.push(Span::styled(next, theme::dim()));
                } else {
                    lines.push(Line::styled(
                        truncate(&format!("  next timer: {}", next_timer_text(s)), w),
                        theme::dim(),
                    ));
                }
            }
            lines.insert(0, clip(v, w));
            lines
        }
    }
}

/// Cut a span list to `w` characters.
fn clip(spans: Vec<Span<'static>>, w: usize) -> Line<'static> {
    let mut out = Vec::new();
    let mut used = 0;
    for s in spans {
        let n = s.content.chars().count();
        if used + n <= w {
            used += n;
            out.push(s);
        } else {
            let room = w.saturating_sub(used);
            out.push(Span::styled(truncate(&s.content, room), s.style));
            break;
        }
    }
    Line::from(out)
}

fn failed_lines(s: &Systemd, room: usize, w: usize) -> Vec<Line<'static>> {
    let shown = s.failed.len().min(room.min(MAX_FAILED_LINES));
    let mut out: Vec<Line<'static>> = s
        .failed
        .iter()
        .take(shown)
        .map(|u| {
            let what = if u.reason.is_empty() {
                "failed".to_string()
            } else {
                format!("failed ({})", u.reason)
            };
            let when = if u.since.is_empty() {
                String::new()
            } else {
                format!(" · {}", u.since)
            };
            let user = if u.user { " (user)" } else { "" };
            Line::styled(
                truncate(&format!("✖ {}{user} — {what}{when}", u.name), w),
                theme::bold(CRIT),
            )
        })
        .collect();
    let more = s.failed.len() - shown;
    if more > 0
        && let Some(last) = out.last_mut()
    {
        *last = Line::styled(
            truncate(&format!("✖ …and {} more failed units", more + 1), w),
            theme::bold(CRIT),
        );
    }
    out
}

fn container_lines(state: &ContainersState, tall: bool, w: usize) -> Vec<Line<'static>> {
    let head = |engine: &str, text: String, st| {
        Line::from(vec![
            Span::styled(format!("{engine}: "), theme::dim()),
            Span::styled(truncate(&text, w.saturating_sub(engine.len() + 2)), st),
        ])
    };
    match state {
        ContainersState::Pending => vec![head("Docker", "checking…".into(), theme::muted())],
        ContainersState::NotInstalled => {
            vec![head("Docker", "not installed".into(), theme::muted())]
        }
        ContainersState::NoAccess => vec![head(
            "Docker",
            format!("no access — {NO_ACCESS_HINT}"),
            theme::bold(WARN),
        )],
        ContainersState::Error(e) => vec![head("Containers", e.clone(), theme::bold(WARN))],
        ContainersState::Ready { engine, list } => {
            let running = list.iter().filter(|c| c.state == "running").count();
            let stopped = list.len() - running;
            let names: Vec<&str> = list
                .iter()
                .filter(|c| c.state == "running")
                .map(|c| c.name.as_str())
                .collect();
            let mut text = format!("{running} running · {stopped} stopped");
            if !tall && !names.is_empty() {
                text.push_str(&format!(" — {}", names.join(", ")));
            }
            let mut out = vec![head(engine, text, theme::text())];
            if tall {
                for c in list.iter().take(MAX_CONTAINER_LINES) {
                    let stats = match (c.cpu_pct, c.mem) {
                        (Some(p), Some(m)) => format!(" · {p:.1}% · {}", fmt::bytes(m)),
                        _ => String::new(),
                    };
                    let up = c.state == "running";
                    let st = if up { theme::text() } else { theme::muted() };
                    out.push(Line::styled(
                        truncate(
                            &format!(
                                "  {} {} — {} — {}{stats}",
                                if up { "●" } else { "○" },
                                c.name,
                                c.image,
                                c.status
                            ),
                            w,
                        ),
                        st,
                    ));
                }
            }
            out
        }
    }
}

fn alert_line(a: &Alert, w: usize) -> Line<'static> {
    let (sym, color) = match (a.kind, a.sev) {
        (Kind::Clear, _) => ("✓", OK),
        (_, Severity::Critical) => ("✖", CRIT),
        (_, Severity::Warn) => ("▲", WARN),
        (_, Severity::Info) => ("•", ACCENT_CPU),
    };
    let text = truncate(&a.message, w.saturating_sub(8));
    Line::from(vec![
        Span::styled(format!("{} ", local_hm(a.at)), theme::dim()),
        Span::styled(format!("{sym} "), theme::bold(color)),
        Span::styled(
            text,
            if a.kind == Kind::Clear {
                theme::dim()
            } else {
                theme::text()
            },
        ),
    ])
}

pub fn draw(f: &mut Frame, area: Rect, app: &App, snap: &Snapshot) {
    let failed = failed_count(snap);
    let active = app.alerts.active_count();
    let headline = if failed > 0 {
        (format!("{failed} failed"), theme::bold(CRIT))
    } else if active > 0 {
        (
            format!("{active} alert{}", if active == 1 { "" } else { "s" }),
            theme::bold(WARN),
        )
    } else {
        ("all OK".to_string(), theme::bold(OK))
    };
    let block = panel_block(
        Panel::Services.num(),
        "Services & Alerts",
        Some(headline),
        ACCENT_SVC,
        app.focus == Panel::Services,
        area.width,
    );
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width < 20 || inner.height == 0 {
        return;
    }
    let (w, h) = (inner.width as usize, inner.height as usize);
    let mut lines: Vec<Line<'static>> = summary_lines(&snap.services.systemd, w);
    let top = lines.len();
    let tall = h >= 14;
    let cont = container_lines(&snap.services.containers, tall, w);
    if let SystemdState::Ready(s) = &snap.services.systemd {
        // Keep a line each for the containers and for alerts when there is room at all.
        let room = h.saturating_sub(top + cont.len().min(1) + 1);
        lines.extend(failed_lines(s, room, w));
    }
    let cont_room = h.saturating_sub(lines.len());
    lines.extend(cont.into_iter().take(cont_room));
    let left = h.saturating_sub(lines.len());
    if left > 0 {
        let mut it = app.alerts.recent().peekable();
        if it.peek().is_none() {
            lines.push(Line::styled("No alerts yet — all quiet", theme::muted()));
        }
        lines.extend(it.take(left).map(|a| alert_line(a, w)));
    }
    f.render_widget(Paragraph::new(lines), inner);
}
