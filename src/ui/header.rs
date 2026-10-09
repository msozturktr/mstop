//! Top status line: health chips on the left, host info and clock on the right.

use super::theme::{self, *};
use crate::collect::Snapshot;
use crate::fmt;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

pub struct Chip {
    pub sev: Sev,
    pub text: String,
    /// Overrides the default severity symbol.
    pub icon: Option<&'static str>,
}

impl Chip {
    fn new(sev: Sev, text: impl Into<String>) -> Self {
        Chip {
            sev,
            text: text.into(),
            icon: None,
        }
    }
    fn icon(mut self, icon: &'static str) -> Self {
        self.icon = Some(icon);
        self
    }
}

/// Chip builders, in display order.
const BUILDERS: &[fn(&Snapshot) -> Vec<Chip>] = &[
    sys_chips,
    gpu_chips,
    battery_chips,
    fan_chips,
    profile_chips,
    temp_alert_chips,
    smart_chips,
    disk_full_chips,
    io_chips,
    net_chips,
    svcs_chips,
];

pub fn chips(s: &Snapshot) -> Vec<Chip> {
    BUILDERS.iter().flat_map(|b| b(s)).collect()
}

fn sys_chips(s: &Snapshot) -> Vec<Chip> {
    let mut v = Vec::new();
    let cpu = s.cpu.total;
    let sev = Sev::from_value(cpu, 70.0, 90.0);
    if sev != Sev::Ok {
        v.push(Chip::new(
            sev,
            format!(
                "SYS: CPU {} {:.0}%",
                if sev == Sev::Crit {
                    "saturated"
                } else {
                    "busy"
                },
                cpu
            ),
        ));
    }
    if s.mem.total > 0 {
        let mp = s.mem.used as f64 / s.mem.total as f64 * 100.0;
        let sev = Sev::from_value(mp, 80.0, 92.0);
        if sev != Sev::Ok {
            v.push(Chip::new(sev, format!("SYS: memory pressure {mp:.0}%")));
        }
    }
    if s.mem.swap_total > 0 {
        let sp = s.mem.swap_used as f64 / s.mem.swap_total as f64 * 100.0;
        let sev = Sev::from_value(sp, 25.0, 60.0);
        if sev != Sev::Ok {
            v.push(Chip::new(sev, format!("SYS: swap in use {sp:.0}%")));
        }
    }
    if v.is_empty() {
        v.push(Chip::new(Sev::Ok, "SYS: OK"));
    }
    v
}

fn svcs_chips(s: &Snapshot) -> Vec<Chip> {
    match super::services::failed_count(s) {
        0 => Vec::new(),
        n => vec![Chip::new(Sev::Crit, format!("SVCS: {n} failed")).icon("✖")],
    }
}

/// Count of active warn/critical alerts; hidden when none.
fn alert_chips(active: usize) -> Vec<Chip> {
    if active == 0 {
        Vec::new()
    } else {
        vec![Chip::new(Sev::Warn, format!("ALERTS: {active}")).icon("▲")]
    }
}

fn gpu_chips(s: &Snapshot) -> Vec<Chip> {
    let Some(g) = s
        .gpus
        .iter()
        .find(|g| g.discrete)
        .or_else(|| s.gpus.first())
    else {
        return Vec::new();
    };
    // Same numbering as the GPU panel and `--dump`.
    let name = if s.gpus.len() > 1 {
        format!("GPU {}", g.index)
    } else {
        "GPU".to_string()
    };
    if g.sleeping {
        return vec![Chip::new(Sev::Ok, format!("{name}: asleep"))];
    }
    match g.temp_c {
        Some(t) => vec![Chip::new(
            Sev::from_value(t, 70.0, 85.0),
            format!("{name}: {t:.0}°C"),
        )],
        None => vec![Chip::new(Sev::Ok, format!("{name}: active"))],
    }
}

fn battery_chips(s: &Snapshot) -> Vec<Chip> {
    let Some(b) = &s.battery else {
        return Vec::new();
    };
    let ac = b.ac_online.unwrap_or(false);
    if b.discharging() && !ac {
        let sev = if b.capacity < 10.0 {
            Sev::Crit
        } else if b.capacity < 20.0 {
            Sev::Warn
        } else {
            Sev::Ok
        };
        let left = b
            .time_left_secs
            .map(|t| format!(" {}h{:02}m", t / 3600, t % 3600 / 60))
            .unwrap_or_default();
        vec![Chip::new(sev, format!("BAT: {:.0}%{left}", b.capacity)).icon("▼")]
    } else {
        let tail = if ac { " AC" } else { "" };
        vec![Chip::new(Sev::Ok, format!("BAT: {:.0}%{tail}", b.capacity)).icon("⚡")]
    }
}

fn fan_chips(s: &Snapshot) -> Vec<Chip> {
    let max = s
        .sensors
        .chips
        .iter()
        .flat_map(|c| c.fans.iter().map(|f| f.rpm))
        .max();
    match max {
        Some(r) => {
            vec![Chip::new(Sev::Ok, format!("FAN: {} RPM", fmt::thousands(r as u64))).icon("✦")]
        }
        None => Vec::new(),
    }
}

fn profile_chips(s: &Snapshot) -> Vec<Chip> {
    s.power_profile
        .as_ref()
        .map(|p| vec![Chip::new(Sev::Ok, format!("PROFILE: {p}")).icon("◆")])
        .unwrap_or_default()
}

fn temp_alert_chips(s: &Snapshot) -> Vec<Chip> {
    match super::sensors::hottest_alert(s) {
        Some((chip, label, c, sev)) => {
            vec![Chip::new(sev, format!("TEMP: {chip} {label} {c:.0}°C"))]
        }
        None => Vec::new(),
    }
}

fn smart_chips(s: &Snapshot) -> Vec<Chip> {
    use crate::collect::disks::SmartState;
    let mut any = false;
    for d in &s.disks.disks {
        if let SmartState::Ready(m) = &d.smart {
            any = true;
            if m.passed == Some(false) {
                return vec![Chip::new(Sev::Crit, format!("SMART: {} failing", d.name))];
            }
        }
    }
    if any {
        vec![Chip::new(Sev::Ok, "SMART: OK").icon("✓")]
    } else {
        Vec::new()
    }
}

fn disk_full_chips(s: &Snapshot) -> Vec<Chip> {
    let worst = s
        .disks
        .mounts
        .iter()
        .filter(|m| super::disks::full_sev(m) != Sev::Ok)
        .max_by(|a, b| a.used_pct().total_cmp(&b.used_pct()));
    match worst {
        Some(m) => vec![Chip::new(
            super::disks::full_sev(m),
            format!("DISK: {} {:.0}% full", m.points[0], m.used_pct()),
        )],
        None => Vec::new(),
    }
}

fn io_chips(s: &Snapshot) -> Vec<Chip> {
    let total = s.disks.read_bps() + s.disks.write_bps();
    if total > 1_000_000.0 {
        vec![Chip::new(Sev::Ok, format!("IO: {}", fmt::rate(total)))]
    } else {
        Vec::new()
    }
}

fn net_chips(s: &Snapshot) -> Vec<Chip> {
    if s.net.ifaces.iter().any(|i| i.up) {
        vec![Chip::new(Sev::Ok, format!("NET: {}", fmt::rate(s.net.rx_bps))).icon("▼")]
    } else {
        Vec::new()
    }
}

pub fn clock() -> String {
    // SAFETY: localtime_r writes into our zeroed tm; time(NULL) is always valid.
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
    }
}

pub fn draw(f: &mut Frame, area: Rect, snap: Option<&Snapshot>, alerts: usize) {
    let mut left: Vec<Span> = vec![Span::raw(" ")];
    let mut right = String::new();
    match snap {
        Some(s) => {
            for c in chips(s).into_iter().chain(alert_chips(alerts)) {
                let symbol = match (c.icon, c.sev) {
                    (_, sev) if sev != Sev::Ok => c.icon.unwrap_or(sev.symbol()),
                    (Some(i), _) => i,
                    _ => "●",
                };
                left.push(Span::styled(
                    format!("{symbol} {}", c.text),
                    theme::bold(c.sev.color()),
                ));
                left.push(Span::raw("  "));
            }
            let up = s.cpu.uptime_secs;
            right = format!(
                "{} · up {}d {}h · {} ",
                s.cpu.governor.as_deref().unwrap_or("n/a"),
                up / 86_400,
                up % 86_400 / 3600,
                clock()
            );
        }
        None => left.push(Span::styled("◌ Collecting first sample…", theme::dim())),
    }
    let used: usize = left.iter().map(|s| s.content.chars().count()).sum();
    let pad = (area.width as usize).saturating_sub(used + right.chars().count());
    left.push(Span::raw(" ".repeat(pad)));
    left.push(Span::styled(right, theme::dim()));
    f.render_widget(
        Paragraph::new(Line::from(left)).style(ratatui::style::Style::default().bg(BG)),
        area,
    );
}
