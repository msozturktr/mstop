//! Sensors panel: temperatures with severity bars, fans, battery, AC and power profile.

use super::theme::{self, *};
use super::widgets::*;
use crate::app::{App, Panel};
use crate::collect::Snapshot;
use crate::collect::sensors::{Battery, Chip, TempSensor};
use crate::fmt;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

/// Warn at (max or crit) - 10 degrees or >= 80, critical at crit or >= 90.
pub fn temp_sev(t: &TempSensor) -> Sev {
    let c = t.celsius;
    if c >= 90.0 || t.crit.is_some_and(|k| c >= k) {
        return Sev::Crit;
    }
    let limit = match (t.max, t.crit) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    if c >= 80.0 || limit.is_some_and(|l| c >= l - 10.0) {
        Sev::Warn
    } else {
        Sev::Ok
    }
}

/// Hottest sensor with warn or critical severity: (chip title, label, celsius, severity).
pub fn hottest_alert(snap: &Snapshot) -> Option<(String, String, f64, Sev)> {
    let mut best: Option<(String, String, f64, Sev)> = None;
    for c in &snap.sensors.chips {
        for t in &c.temps {
            let sev = temp_sev(t);
            if sev != Sev::Ok && best.as_ref().is_none_or(|b| t.celsius > b.2) {
                best = Some((c.title.clone(), t.label.clone(), t.celsius, sev));
            }
        }
    }
    best
}

pub fn hm(secs: u64) -> String {
    let (h, m) = (secs / 3600, secs % 3600 / 60);
    if h > 0 {
        format!("{h} h {m:02} min")
    } else {
        format!("{m} min")
    }
}

const BAR_W: usize = 10;
/// Groups wrap into two columns at most, and only when the panel is wide enough.
const MAX_COLS: usize = 2;

fn entry_width(lw: usize) -> usize {
    lw + 1 + 1 + BAR_W + 1 + 9 + 2 + 3
}

fn temp_line(label: &str, lw: usize, t: &TempSensor) -> Vec<Span<'static>> {
    let sev = temp_sev(t);
    let scale = t.crit.or(t.max).unwrap_or(100.0).max(50.0);
    let mut v = vec![Span::styled(
        format!("{:<lw$} ", truncate(label, lw)),
        theme::dim(),
    )];
    v.push(Span::styled("▕", theme::muted()));
    v.extend(bar(t.celsius / scale, BAR_W, sev.color()));
    v.push(Span::styled("▏", theme::muted()));
    v.push(Span::styled(
        format!(" {:>5.1} °C", t.celsius),
        if sev == Sev::Ok {
            theme::text()
        } else {
            theme::bold(sev.color())
        },
    ));
    v.push(Span::styled(
        format!(" {}", if sev == Sev::Ok { " " } else { sev.symbol() }),
        sev.style(),
    ));
    v
}

fn titled(chips: &[Chip]) -> Vec<String> {
    chips
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let same: Vec<usize> = chips
                .iter()
                .enumerate()
                .filter(|(_, o)| o.title == c.title)
                .map(|(j, _)| j)
                .collect();
            if same.len() > 1 {
                let n = same.iter().position(|j| *j == i).unwrap_or(0) + 1;
                format!("{} {n}", c.title)
            } else {
                c.title.clone()
            }
        })
        .collect()
}

/// Groups of lines: one per chip with temperatures, then one "Fans" group. Every group is
/// a dim bold header on its own line followed by its indented entries.
fn entries(chips: &[Chip], lw: usize) -> Vec<Vec<Vec<Span<'static>>>> {
    let titles = titled(chips);
    let header = |t: String| vec![Span::styled(t, theme::bold(DIM))];
    let mut out = Vec::new();
    for (c, title) in chips.iter().zip(&titles) {
        if c.temps.is_empty() {
            continue;
        }
        let mut g = vec![header(title.clone())];
        for t in c.temps.iter().take(5) {
            g.push(temp_line(&format!("  {}", t.label), lw, t));
        }
        out.push(g);
    }
    let fan_chips = chips.iter().filter(|c| !c.fans.is_empty()).count();
    let mut fans = Vec::new();
    for (c, title) in chips.iter().zip(&titles) {
        for fan in &c.fans {
            let label = if fan_chips > 1 {
                format!("  {title} {}", fan.label)
            } else {
                format!("  {}", fan.label)
            };
            fans.push(vec![
                Span::styled(format!("{:<lw$} ", truncate(&label, lw)), theme::dim()),
                Span::styled(
                    format!("{} RPM", fmt::thousands(fan.rpm as u64)),
                    theme::text(),
                ),
            ]);
        }
    }
    if !fans.is_empty() {
        let mut g = vec![header("Fans".to_string())];
        g.extend(fans);
        out.push(g);
    }
    out
}

/// Place whole groups into `cols` columns; returns one span list per row, padded to `ew`.
fn flow(groups: Vec<Vec<Vec<Span<'static>>>>, cols: usize, ew: usize) -> Vec<Vec<Span<'static>>> {
    let total: usize = groups.iter().map(Vec::len).sum();
    let longest = groups.iter().map(Vec::len).max().unwrap_or(0);
    let rows = longest.max(total.div_ceil(cols)).max(1);
    let mut columns: Vec<Vec<Vec<Span<'static>>>> = vec![Vec::new()];
    for g in groups {
        let last = columns.last().map_or(0, Vec::len);
        if last > 0 && last + g.len() > rows && columns.len() < cols {
            columns.push(Vec::new());
        }
        columns.last_mut().unwrap().extend(g);
    }
    let nrows = columns.iter().map(Vec::len).max().unwrap_or(0);
    let mut lines: Vec<Vec<Span<'static>>> = vec![Vec::new(); nrows];
    for col in columns {
        for (r, line) in lines.iter_mut().enumerate() {
            match col.get(r) {
                Some(e) => {
                    let w = spans_width(e);
                    line.extend(e.iter().cloned());
                    line.push(Span::raw(" ".repeat(ew.saturating_sub(w))));
                }
                None => line.push(Span::raw(" ".repeat(ew))),
            }
        }
    }
    lines
}

fn label_width(chips: &[Chip]) -> usize {
    let m = chips
        .iter()
        .flat_map(|c| {
            let t = c.title.chars().count() + 2;
            std::iter::once(t)
                .chain(c.temps.iter().map(|x| x.label.chars().count() + 2))
                .chain(c.fans.iter().map(|x| x.label.chars().count() + 2))
        })
        .max()
        .unwrap_or(8);
    m.clamp(8, 18)
}

pub fn battery_lines(b: Option<&Battery>, profile: Option<&str>) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let sep = || Span::styled(" · ", theme::muted());
    match b {
        Some(b) => {
            let sev = if b.discharging() {
                Sev::from_value(100.0 - b.capacity, 80.0, 90.0)
            } else {
                Sev::Ok
            };
            let mut l = vec![
                Span::styled("Battery: ", theme::dim()),
                Span::styled(format!("{:.0}%", b.capacity), theme::bold(sev.color())),
                sep(),
            ];
            let rate = b.rate_w.map(|w| format!(" {w:.1} W")).unwrap_or_default();
            if b.discharging() || b.charging() {
                l.push(Span::styled(format!("{}{rate}", b.status), theme::text()));
            } else {
                l.push(Span::styled(b.status.clone(), theme::text()));
            }
            if let Some(t) = b.time_left_secs {
                l.push(sep());
                let what = if b.discharging() { "left" } else { "to full" };
                l.push(Span::styled(format!("{} {what}", hm(t)), theme::text()));
            }
            if let Some(h) = b.health_pct() {
                l.push(sep());
                l.push(Span::styled(format!("health {h:.0}%"), theme::text()));
            }
            if let Some(c) = b.cycles {
                l.push(sep());
                l.push(Span::styled(format!("{c} cycles"), theme::text()));
            }
            out.push(Line::from(l));
        }
        None => out.push(Line::styled("Battery: none detected", theme::muted())),
    }
    let mut l = Vec::new();
    if let Some(ac) = b.and_then(|b| b.ac_online) {
        l.push(Span::styled("AC adapter: ", theme::dim()));
        l.push(Span::styled(
            if ac { "plugged in" } else { "unplugged" },
            theme::text(),
        ));
    }
    if let Some(p) = profile {
        if !l.is_empty() {
            l.push(sep());
        }
        l.push(Span::styled("Power profile: ", theme::dim()));
        l.push(Span::styled(p.to_string(), theme::text()));
    }
    if !l.is_empty() {
        out.push(Line::from(l));
    }
    out
}

/// Lines needed for the temperature/fan list in `cols` columns.
pub fn rows_needed(snap: &Snapshot, width: usize) -> usize {
    let lw = label_width(&snap.sensors.chips);
    let cols = (width / entry_width(lw)).clamp(1, MAX_COLS);
    flow(entries(&snap.sensors.chips, lw), cols, entry_width(lw)).len()
}

pub fn draw(f: &mut Frame, area: Rect, app: &App, snap: &Snapshot) {
    let hot = hottest_alert(snap);
    let headline = match (&hot, snap.sensors.cpu_temp) {
        (Some((_, _, c, sev)), _) => Some((format!("hottest {c:.0} °C"), theme::bold(sev.color()))),
        (None, Some(c)) => Some((format!("CPU {c:.0} °C"), theme::bold(OK))),
        _ => None,
    };
    let block = panel_block(
        Panel::Sensors.num(),
        "Sensors · temperatures, fans, battery",
        headline,
        ACCENT_SENSORS,
        app.focus == Panel::Sensors,
        area.width,
    );
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width < 24 || inner.height == 0 {
        return;
    }
    let bat = battery_lines(snap.battery.as_ref(), snap.power_profile.as_deref());
    let bat_h = (bat.len() as u16).min(inner.height);
    let chips = &snap.sensors.chips;
    let lw = label_width(chips);
    let ew = entry_width(lw);
    let cols = (inner.width as usize / ew).clamp(1, MAX_COLS);
    let rows: Vec<Line> = flow(entries(chips, lw), cols, ew)
        .into_iter()
        .map(Line::from)
        .collect();
    let list_h = rows.len().min((inner.height - bat_h) as usize);
    if list_h > 0 {
        f.render_widget(
            Paragraph::new(rows.into_iter().take(list_h).collect::<Vec<_>>()),
            Rect::new(inner.x, inner.y, inner.width, list_h as u16),
        );
    }
    let bat_y = inner.y + list_h as u16;
    f.render_widget(
        Paragraph::new(bat),
        Rect::new(
            inner.x,
            bat_y,
            inner.width,
            bat_h.min(inner.height - list_h as u16),
        ),
    );
    // Spare room under the list: CPU temperature history.
    let used = list_h as u16 + bat_h;
    let rest = inner.height.saturating_sub(used);
    if rest >= 7 && snap.sensors.cpu_temp.is_some() {
        let y = inner.y + used;
        f.render_widget(
            Paragraph::new(Line::styled("CPU temperature history", theme::dim())),
            Rect::new(inner.x, y + 1, inner.width, 1),
        );
        let data = app.cpu_temp.view(app.span, app.interval_ms);
        let fmt_y = |v: f64| format!("{v:.0}°");
        let g = Graph {
            data: &data,
            max: 100.0,
            color: ACCENT_SENSORS,
            ylabel: &fmt_y,
            span_secs: app.span_secs(),
        };
        render_graph(
            f.buffer_mut(),
            Rect::new(inner.x, y + 2, inner.width, rest - 2),
            &g,
        );
    }
}
