//! Network panel: primary link, other interfaces, totals, sockets, traffic graph and the
//! apps with the most connections.

use super::theme::{self, *};
use super::widgets::*;
use crate::app::{App, Panel};
use crate::collect::Snapshot;
use crate::collect::net::{Iface, IfaceKind};
use crate::fmt;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

/// Wi-Fi signal strength in words.
pub fn signal_word(dbm: i32) -> &'static str {
    if dbm >= -55 {
        "good"
    } else if dbm >= -70 {
        "fair"
    } else {
        "weak"
    }
}

fn mbps(v: u32) -> String {
    format!("{} Mbps", fmt::thousands(v as u64))
}

/// `Wi-Fi · 1,201 Mbps · signal -44 dBm (good) · 192.168.1.142/24`
fn describe(i: &Iface) -> String {
    if !i.up {
        return if i.kind == IfaceKind::Ethernet {
            "Disconnected (cable unplugged)".into()
        } else {
            "Disconnected".into()
        };
    }
    let mut parts = vec![i.kind.label().to_string()];
    if let Some(s) = i.speed_mbps {
        parts.push(mbps(s));
    }
    if let Some(d) = i.signal_dbm {
        parts.push(format!("signal {d} dBm ({})", signal_word(d)));
    }
    if let Some(ip) = i.ipv4.first() {
        parts.push(ip.clone());
    }
    parts.join(" · ")
}

fn compact(i: &Iface) -> String {
    if !i.up {
        return describe(i);
    }
    let mut parts = vec![i.kind.label().to_string()];
    parts.push(format!(
        "↓ {} · ↑ {}",
        fmt::rate(i.rx_bps),
        fmt::rate(i.tx_bps)
    ));
    if let Some(ip) = i.ipv4.first() {
        parts.push(ip.clone());
    }
    parts.join(" · ")
}

fn plain(s: String) -> Vec<Span<'static>> {
    vec![Span::styled(s, theme::text())]
}

/// Space between the longest label and the values; tight so the primary line fits.
const GAP: usize = 1;

/// Status rows above the graph.
pub fn lines(snap: &Snapshot, w: usize) -> Vec<Line<'static>> {
    let n = &snap.net;
    let primary = n.primary();
    let mut items: Vec<(String, String)> = Vec::new();
    match primary {
        Some(p) => items.push((format!("Primary ({}):", p.name), describe(p))),
        None => items.push(("Primary:".into(), "No active connection".into())),
    }
    items.push((
        "Gateway:".into(),
        n.gateway
            .as_ref()
            .map(|g| g.ip.clone())
            .unwrap_or_else(|| "none".into()),
    ));
    let others: Vec<&Iface> = n
        .ifaces
        .iter()
        .filter(|i| Some(i.name.as_str()) != primary.map(|p| p.name.as_str()))
        .filter(|i| i.kind != IfaceKind::Virtual || i.up)
        .collect();
    for i in others.iter().take(4) {
        items.push((format!("{}:", i.name), compact(i)));
    }
    if others.len() > 4 {
        items.push((
            "…".into(),
            format!("and {} more interfaces", others.len() - 4),
        ));
    }
    let shown = n
        .ifaces
        .iter()
        .filter(|i| matches!(i.kind, IfaceKind::Wifi | IfaceKind::Ethernet));
    let (rx, tx, err, drop) = shown.fold((0u64, 0u64, 0u64, 0u64), |a, i| {
        (
            a.0 + i.rx_total,
            a.1 + i.tx_total,
            a.2 + i.errors,
            a.3 + i.drops,
        )
    });
    let mut boot = format!(
        "↓ {} · ↑ {} · {} error{}",
        fmt::bytes(rx),
        fmt::bytes(tx),
        err,
        if err == 1 { "" } else { "s" }
    );
    if drop > 0 {
        boot.push_str(&format!(" · {drop} dropped"));
    }
    items.push(("Since boot:".into(), boot));
    let k = &n.sockets;
    items.push((
        "Connections:".into(),
        format!(
            "{} open ({} established, {} listening) · UDP {}",
            k.tcp_total(),
            k.established,
            k.listen,
            k.udp
        ),
    ));
    let lw = items
        .iter()
        .map(|(l, _)| l.chars().count())
        .max()
        .unwrap_or(0)
        + 3;
    let vw = w.saturating_sub(lw);
    kv_lines_gap(
        items
            .into_iter()
            .map(|(l, v)| {
                let v = if v.starts_with("Disconnected") {
                    vec![Span::styled(truncate(&v, vw), theme::muted())]
                } else {
                    plain(truncate(&v, vw))
                };
                (l, v)
            })
            .collect(),
        GAP,
    )
}

/// "Most connections" block.
pub fn apps_lines(snap: &Snapshot, w: usize) -> Vec<Line<'static>> {
    let mut out = vec![Line::styled(
        truncate("Most connections (per-app traffic needs root)", w),
        theme::dim(),
    )];
    if snap.net.apps.is_empty() {
        out.push(Line::styled("  (none visible)", theme::muted()));
    }
    let nw = w.saturating_sub(2 + 1 + 15).clamp(8, 18);
    for a in &snap.net.apps {
        let ports = if a.listening.is_empty() {
            String::new()
        } else {
            let p: Vec<String> = a.listening.iter().take(3).map(|p| p.to_string()).collect();
            format!(" · listening {}", p.join(", "))
        };
        out.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("{:<nw$}", truncate(&a.name, nw)), theme::text()),
            Span::styled(
                truncate(
                    &format!(" {:>3} established{ports}", a.established),
                    w.saturating_sub(2 + nw),
                ),
                theme::dim(),
            ),
        ]));
    }
    out
}

pub fn draw(f: &mut Frame, area: Rect, app: &App, snap: &Snapshot) {
    let n = &snap.net;
    let title = match n.primary() {
        Some(p) => format!("Network · {}", p.name),
        None => "Network".to_string(),
    };
    let headline = Some((
        format!("↓ {} · ↑ {}", fmt::rate(n.rx_bps), fmt::rate(n.tx_bps)),
        theme::bold(ACCENT_NET),
    ));
    let block = panel_block(
        Panel::Network.num(),
        &title,
        headline,
        ACCENT_NET,
        app.focus == Panel::Network,
        area.width,
    );
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width < 20 || inner.height == 0 {
        return;
    }
    let w = inner.width as usize;
    let ls = lines(snap, w);
    let used = (ls.len() as u16).min(inner.height);
    f.render_widget(
        Paragraph::new(ls),
        Rect::new(inner.x, inner.y, inner.width, used),
    );
    let rest = inner.height - used;
    let apps = apps_lines(snap, w);
    // The graph (legend plus at least 4 rows) keeps priority over a long app list, but the
    // list always keeps its header and two entries.
    let graph_h = if rest >= 5 + 3 {
        rest.saturating_sub(apps.len() as u16).max(5)
    } else {
        0
    };
    let apps_h = (apps.len() as u16).min(rest - graph_h);
    if graph_h >= 5 {
        let legend = Line::from(vec![
            Span::styled("Traffic  ", theme::dim()),
            Span::styled("↓ download", theme::bold(ACCENT_NET)),
            Span::styled("  ", theme::dim()),
            Span::styled("↑ upload", theme::bold(ACCENT_NET_UP)),
        ]);
        f.render_widget(
            Paragraph::new(legend),
            Rect::new(inner.x, inner.y + used, inner.width, 1),
        );
        let down = app.net_down.view(app.span, app.interval_ms);
        let up = app.net_up.view(app.span, app.interval_ms);
        let peak = down.iter().chain(&up).cloned().fold(0.0, f64::max);
        let max = nice_max(peak, 100_000.0);
        let series = [
            Series {
                data: &down,
                color: ACCENT_NET,
            },
            Series {
                data: &up,
                color: ACCENT_NET_UP,
            },
        ];
        let fmt_y = |v: f64| fmt::rate(v);
        render_series(
            f.buffer_mut(),
            Rect::new(inner.x, inner.y + used + 1, inner.width, graph_h - 1),
            &series,
            max,
            &fmt_y,
            app.span_secs(),
        );
    }
    if apps_h > 0 {
        f.render_widget(
            Paragraph::new(apps),
            Rect::new(
                inner.x,
                inner.y + inner.height - apps_h,
                inner.width,
                apps_h,
            ),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signal_words() {
        assert_eq!(signal_word(-44), "good");
        assert_eq!(signal_word(-55), "good");
        assert_eq!(signal_word(-56), "fair");
        assert_eq!(signal_word(-70), "fair");
        assert_eq!(signal_word(-71), "weak");
    }
}
