//! Alert rule engine: hysteresis plus a minimum duration so alerts do not flap. Each alert
//! is logged once when it starts and once when it clears.

use crate::collect::Snapshot;
use crate::collect::services::SystemdState;
use crate::fmt;
use std::collections::{HashMap, HashSet, VecDeque};

// ---- Thresholds (all rules live here) ---------------------------------------------------

pub const LOG_CAP: usize = 100;
/// Seconds a "sustained" condition must hold before it fires.
pub const HOLD_S: f64 = 10.0;

pub const CPU_TEMP_FIRE: f64 = 90.0;
pub const CPU_TEMP_CLEAR: f64 = 85.0;
pub const GPU_TEMP_FIRE: f64 = 85.0;
pub const GPU_TEMP_CLEAR: f64 = 80.0;
/// Alert when a sensor is within this many degrees of its critical limit.
pub const SENSOR_CRIT_MARGIN: f64 = 5.0;
/// Extra degrees below the fire point before a sensor alert clears.
pub const SENSOR_CLEAR_EXTRA: f64 = 3.0;
pub const MEM_FIRE_PCT: f64 = 90.0;
pub const MEM_CLEAR_PCT: f64 = 85.0;
pub const SWAP_FIRE_PCT: f64 = 50.0;
pub const SWAP_CLEAR_PCT: f64 = 40.0;
pub const MOUNT_WARN_PCT: f64 = 90.0;
pub const MOUNT_CRIT_PCT: f64 = 95.0;
pub const MOUNT_CLEAR_PCT: f64 = 88.0;
pub const BATTERY_WARN_PCT: f64 = 15.0;
pub const BATTERY_CRIT_PCT: f64 = 5.0;
pub const BATTERY_CLEAR_PCT: f64 = 18.0;
pub const PROC_FIRE_PCT: f64 = 95.0;
pub const PROC_CLEAR_PCT: f64 = 80.0;
/// Processes above this are tracked so a dip below the fire line does not drop them.
pub const PROC_TRACK_PCT: f64 = 50.0;
pub const PROC_HOLD_S: f64 = 60.0;

// ---- Types ------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Severity {
    Info,
    Warn,
    Critical,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Start,
    Clear,
    /// One-shot notice (no matching clear).
    Event,
}

#[derive(Clone, Debug)]
pub struct Alert {
    /// Unix seconds.
    pub at: u64,
    pub sev: Severity,
    pub kind: Kind,
    pub message: String,
}

/// One rule evaluated against the current snapshot.
struct Cond {
    key: String,
    /// Past the firing threshold.
    trip: bool,
    /// Past the clearing threshold (the two differ: hysteresis).
    clear: bool,
    hold: f64,
    sev: Severity,
    fire: String,
    back: String,
}

#[derive(Default)]
struct State {
    pending: Option<f64>,
    active: bool,
    sev: Option<Severity>,
    back: String,
}

#[derive(Default)]
pub struct Engine {
    states: HashMap<String, State>,
    log: VecDeque<Alert>,
    ifaces: Option<HashMap<String, bool>>,
    /// Units that were already failed when first seen (not "newly" failed).
    failed_baseline: Option<HashSet<String>>,
}

pub fn local_hm(epoch: u64) -> String {
    // SAFETY: localtime_r writes into our zeroed tm; the time value is passed by pointer.
    unsafe {
        let t = epoch as libc::time_t;
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
    }
}

impl Engine {
    pub fn new() -> Self {
        Self::default()
    }

    /// Newest first.
    pub fn recent(&self) -> impl Iterator<Item = &Alert> {
        self.log.iter().rev()
    }

    /// Active alerts of warn or critical severity.
    pub fn active_count(&self) -> usize {
        self.states
            .values()
            .filter(|s| s.active && s.sev.is_some_and(|v| v >= Severity::Warn))
            .count()
    }

    /// Plain-text summary for `--dump`.
    pub fn dump(&self) -> String {
        use std::fmt::Write;
        let mut s = format!(
            "alerts: {} active, {} logged\n",
            self.active_count(),
            self.log.len()
        );
        for a in self.recent() {
            let _ = writeln!(
                s,
                "alert {} {:?} {:?}: {}",
                local_hm(a.at),
                a.sev,
                a.kind,
                a.message
            );
        }
        s
    }

    fn push(&mut self, wall: u64, sev: Severity, kind: Kind, message: String) {
        if self.log.len() == LOG_CAP {
            self.log.pop_front();
        }
        self.log.push_back(Alert {
            at: wall,
            sev,
            kind,
            message,
        });
    }

    /// Evaluate every rule. `now` is a monotonic clock in seconds, `wall` the Unix time.
    pub fn evaluate(&mut self, snap: &Snapshot, now: f64, wall: u64) {
        let mut conds = Vec::new();
        sustained_rules(snap, &mut conds);
        capacity_rules(snap, &mut conds);
        self.failed_rules(snap, &mut conds);
        self.apply(conds, now, wall);
        self.iface_events(snap, wall);
    }

    fn apply(&mut self, conds: Vec<Cond>, now: f64, wall: u64) {
        let mut seen: HashSet<String> = HashSet::new();
        let mut events: Vec<(Severity, Kind, String)> = Vec::new();
        for c in conds {
            seen.insert(c.key.clone());
            let st = self.states.entry(c.key).or_default();
            st.back = c.back;
            if !st.active {
                if !c.trip {
                    st.pending = None;
                    continue;
                }
                let since = *st.pending.get_or_insert(now);
                if now - since >= c.hold {
                    st.active = true;
                    st.pending = None;
                    st.sev = Some(c.sev);
                    events.push((c.sev, Kind::Start, c.fire));
                }
            } else if c.clear {
                st.active = false;
                st.pending = None;
                events.push((st.sev.take().unwrap_or(c.sev), Kind::Clear, st.back.clone()));
            } else if c.trip && st.sev.is_some_and(|s| c.sev > s) {
                st.sev = Some(c.sev);
                events.push((c.sev, Kind::Start, c.fire));
            }
        }
        // A rule whose subject vanished (unplugged sensor, exited process, unmounted disk)
        // clears instead of staying active forever.
        for (key, st) in self.states.iter_mut() {
            if !seen.contains(key) {
                st.pending = None;
                if st.active {
                    st.active = false;
                    events.push((
                        st.sev.take().unwrap_or(Severity::Info),
                        Kind::Clear,
                        st.back.clone(),
                    ));
                }
            }
        }
        self.states.retain(|_, s| s.active || s.pending.is_some());
        for (sev, kind, msg) in events {
            self.push(wall, sev, kind, msg);
        }
    }

    /// Failed systemd units: only units that failed after mstop started count as new.
    fn failed_rules(&mut self, snap: &Snapshot, conds: &mut Vec<Cond>) {
        let SystemdState::Ready(sd) = &snap.services.systemd else {
            return;
        };
        let base = self.failed_baseline.get_or_insert_with(|| {
            sd.failed
                .iter()
                .map(|u| format!("{}{}", u.name, u.user))
                .collect()
        });
        let now_failed: HashSet<String> = sd
            .failed
            .iter()
            .map(|u| format!("{}{}", u.name, u.user))
            .collect();
        base.retain(|k| now_failed.contains(k));
        for u in &sd.failed {
            let id = format!("{}{}", u.name, u.user);
            if base.contains(&id) {
                continue;
            }
            let why = if u.reason.is_empty() {
                String::new()
            } else {
                format!(" ({})", u.reason)
            };
            conds.push(Cond {
                key: format!("unit:{id}"),
                trip: true,
                clear: false,
                hold: 0.0,
                sev: Severity::Warn,
                fire: format!("Service {} failed{why}", u.name),
                back: format!("Service {} is no longer failed", u.name),
            });
        }
    }

    /// Interfaces going up or down (info events, no clear).
    fn iface_events(&mut self, snap: &Snapshot, wall: u64) {
        let cur: HashMap<String, bool> = snap
            .net
            .ifaces
            .iter()
            .filter(|i| i.name != "lo")
            .map(|i| (i.name.clone(), i.up))
            .collect();
        if let Some(prev) = &self.ifaces {
            let mut msgs = Vec::new();
            let mut names: Vec<_> = cur.keys().collect();
            names.sort();
            for n in names {
                if let Some(was) = prev.get(n)
                    && *was != cur[n]
                {
                    let state = if cur[n] { "came up" } else { "went down" };
                    msgs.push(format!("Network interface {n} {state}"));
                }
            }
            for m in msgs {
                self.push(wall, Severity::Info, Kind::Event, m);
            }
        }
        self.ifaces = Some(cur);
    }
}

fn pct(used: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        used as f64 / total as f64 * 100.0
    }
}

/// Rules that must hold for a while before they fire.
fn sustained_rules(snap: &Snapshot, out: &mut Vec<Cond>) {
    if let Some(t) = snap.sensors.cpu_temp {
        out.push(Cond {
            key: "cpu:temp".into(),
            trip: t >= CPU_TEMP_FIRE,
            clear: t < CPU_TEMP_CLEAR,
            hold: HOLD_S,
            sev: Severity::Warn,
            fire: format!(
                "CPU temperature {t:.0} °C (above {CPU_TEMP_FIRE:.0} °C for {HOLD_S:.0} s)"
            ),
            back: format!("CPU temperature back to normal ({t:.0} °C)"),
        });
    }
    for g in snap.gpus.iter().filter(|g| !g.sleeping) {
        if let Some(t) = g.temp_c {
            let name = if snap.gpus.len() > 1 {
                format!("GPU {}", g.index)
            } else {
                "GPU".to_string()
            };
            out.push(Cond {
                key: format!("gpu{}:temp", g.index),
                trip: t >= GPU_TEMP_FIRE,
                clear: t < GPU_TEMP_CLEAR,
                hold: HOLD_S,
                sev: Severity::Warn,
                fire: format!(
                    "{name} temperature {t:.0} °C (above {GPU_TEMP_FIRE:.0} °C for {HOLD_S:.0} s)"
                ),
                back: format!("{name} temperature back to normal ({t:.0} °C)"),
            });
        }
    }
    if snap.mem.total > 0 {
        let p = pct(snap.mem.used, snap.mem.total);
        out.push(Cond {
            key: "mem:used".into(),
            trip: p >= MEM_FIRE_PCT,
            clear: p < MEM_CLEAR_PCT,
            hold: HOLD_S,
            sev: Severity::Warn,
            fire: format!("Memory {p:.0}% used (above {MEM_FIRE_PCT:.0}% for {HOLD_S:.0} s)"),
            back: format!("Memory use back to normal ({p:.0}%)"),
        });
    }
    for p in snap.procs.list.iter().filter(|p| p.cpu >= PROC_TRACK_PCT) {
        out.push(Cond {
            key: format!("proc:{}", p.pid),
            trip: p.cpu >= PROC_FIRE_PCT,
            clear: p.cpu < PROC_CLEAR_PCT,
            hold: PROC_HOLD_S,
            sev: Severity::Info,
            fire: format!(
                "{} (PID {}) has used a full CPU core for {PROC_HOLD_S:.0} s ({:.0}%)",
                p.name, p.pid, p.cpu
            ),
            back: format!("{} (PID {}) no longer pins a CPU core", p.name, p.pid),
        });
    }
}

/// Rules that fire as soon as the value crosses the line.
fn capacity_rules(snap: &Snapshot, out: &mut Vec<Cond>) {
    for chip in &snap.sensors.chips {
        for t in &chip.temps {
            let Some(crit) = t.crit else { continue };
            let line = crit - SENSOR_CRIT_MARGIN;
            out.push(Cond {
                key: format!("sensor:{}:{}", chip.name, t.label),
                trip: t.celsius >= line,
                clear: t.celsius < line - SENSOR_CLEAR_EXTRA,
                hold: 0.0,
                sev: Severity::Warn,
                fire: format!(
                    "{} {} at {:.0} °C, close to its {crit:.0} °C limit",
                    chip.title, t.label, t.celsius
                ),
                back: format!(
                    "{} {} back to normal ({:.0} °C)",
                    chip.title, t.label, t.celsius
                ),
            });
        }
    }
    if snap.mem.swap_total > 0 {
        let p = pct(snap.mem.swap_used, snap.mem.swap_total);
        out.push(Cond {
            key: "mem:swap".into(),
            trip: p >= SWAP_FIRE_PCT,
            clear: p < SWAP_CLEAR_PCT,
            hold: 0.0,
            sev: Severity::Warn,
            fire: format!("Swap {p:.0}% used (above {SWAP_FIRE_PCT:.0}%)"),
            back: format!("Swap use back to normal ({p:.0}%)"),
        });
    }
    for m in &snap.disks.mounts {
        let p = m.used_pct();
        out.push(Cond {
            key: format!("mount:{}", m.label()),
            trip: p >= MOUNT_WARN_PCT,
            clear: p < MOUNT_CLEAR_PCT,
            hold: 0.0,
            sev: if p >= MOUNT_CRIT_PCT {
                Severity::Critical
            } else {
                Severity::Warn
            },
            fire: format!(
                "{} is {p:.0}% full ({} free)",
                m.label(),
                fmt::bytes(m.avail)
            ),
            back: format!("{} is back below {MOUNT_CLEAR_PCT:.0}% full", m.label()),
        });
    }
    for d in &snap.disks.disks {
        if let crate::collect::disks::SmartState::Ready(s) = &d.smart {
            out.push(Cond {
                key: format!("smart:{}", d.name),
                trip: s.passed == Some(false),
                clear: s.passed != Some(false),
                hold: 0.0,
                sev: Severity::Critical,
                fire: format!("Disk {} reports that SMART health is failing", d.name),
                back: format!("Disk {} SMART health is OK again", d.name),
            });
        }
    }
    if let Some(b) = &snap.battery {
        let low = b.discharging() && b.capacity <= BATTERY_WARN_PCT;
        out.push(Cond {
            key: "battery:low".into(),
            trip: low,
            clear: !b.discharging() || b.capacity > BATTERY_CLEAR_PCT,
            hold: 0.0,
            sev: if b.capacity <= BATTERY_CRIT_PCT {
                Severity::Critical
            } else {
                Severity::Warn
            },
            fire: format!("Battery low: {:.0}% and discharging", b.capacity),
            back: format!("Battery no longer low ({:.0}%)", b.capacity),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collect::gpu::Gpu;
    use crate::collect::sensors::{Battery, Chip, Sensors, TempSensor};
    use crate::collect::services::{FailedUnit, Systemd};

    fn snap_temp(cpu: f64) -> Snapshot {
        Snapshot {
            sensors: Sensors {
                cpu_temp: Some(cpu),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn kinds(e: &Engine) -> Vec<(Kind, String)> {
        let mut v: Vec<_> = e.recent().map(|a| (a.kind, a.message.clone())).collect();
        v.reverse();
        v
    }

    #[test]
    fn needs_duration_and_hysteresis() {
        let mut e = Engine::new();
        // 9 seconds hot is not enough.
        for t in 0..9 {
            e.evaluate(&snap_temp(92.0), t as f64, 1000 + t);
        }
        assert!(kinds(&e).is_empty());
        // A dip below the fire line resets the timer.
        e.evaluate(&snap_temp(80.0), 9.0, 1009);
        for t in 10..19 {
            e.evaluate(&snap_temp(92.0), t as f64, 1000 + t);
        }
        assert!(kinds(&e).is_empty());
        e.evaluate(&snap_temp(92.0), 20.0, 1020);
        assert_eq!(kinds(&e).len(), 1);
        assert_eq!(e.active_count(), 1);
        // Inside the hysteresis band (85..90): stays active, nothing new.
        for t in 21..40 {
            e.evaluate(&snap_temp(87.0), t as f64, 1000 + t);
            e.evaluate(&snap_temp(91.0), t as f64 + 0.5, 1000 + t);
        }
        assert_eq!(kinds(&e).len(), 1);
        e.evaluate(&snap_temp(80.0), 41.0, 1041);
        let k = kinds(&e);
        assert_eq!(k.len(), 2);
        assert_eq!(k[0].0, Kind::Start);
        assert_eq!(k[1].0, Kind::Clear);
        assert!(k[0].1.contains("above 90 °C for 10 s"));
        assert!(k[1].1.contains("back to normal"));
        assert_eq!(e.active_count(), 0);
    }

    #[test]
    fn immediate_rules_escalate_and_clear() {
        let mut e = Engine::new();
        let batt = |cap: f64, status: &str| Snapshot {
            battery: Some(Battery {
                capacity: cap,
                status: status.into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        e.evaluate(&batt(50.0, "Discharging"), 0.0, 1);
        assert!(kinds(&e).is_empty());
        e.evaluate(&batt(15.0, "Discharging"), 1.0, 2);
        e.evaluate(&batt(14.0, "Discharging"), 2.0, 3);
        assert_eq!(kinds(&e).len(), 1);
        e.evaluate(&batt(5.0, "Discharging"), 3.0, 4);
        let k: Vec<_> = e.recent().map(|a| a.sev).collect();
        assert_eq!(k, vec![Severity::Critical, Severity::Warn]);
        // Plugging in clears it.
        e.evaluate(&batt(5.0, "Charging"), 4.0, 5);
        assert_eq!(e.recent().next().unwrap().kind, Kind::Clear);
        assert_eq!(e.active_count(), 0);
    }

    #[test]
    fn vanished_subject_clears_and_ring_is_capped() {
        let mut e = Engine::new();
        let mut s = snap_temp(40.0);
        s.sensors.chips = vec![Chip {
            name: "nvme".into(),
            title: "NVMe".into(),
            temps: vec![TempSensor {
                label: "Composite".into(),
                celsius: 82.0,
                max: None,
                crit: Some(85.0),
            }],
            fans: vec![],
        }];
        e.evaluate(&s, 0.0, 1);
        assert_eq!(e.active_count(), 1);
        e.evaluate(&snap_temp(40.0), 1.0, 2);
        assert_eq!(e.active_count(), 0);
        assert_eq!(kinds(&e)[1].0, Kind::Clear);
        // Flapping interface events never exceed the ring.
        let up = |v: bool| {
            let mut s = Snapshot::default();
            s.net.ifaces = vec![crate::collect::net::Iface {
                name: "wlan0".into(),
                up: v,
                ..Default::default()
            }];
            s
        };
        for i in 0..300 {
            e.evaluate(&up(i % 2 == 0), i as f64, i);
        }
        assert_eq!(e.recent().count(), LOG_CAP);
        assert!(e.recent().next().unwrap().message.contains("wlan0"));
    }

    #[test]
    fn new_failed_unit_only_and_gpu_sleep() {
        let unit = |n: &str| FailedUnit {
            name: n.into(),
            reason: "exit code 1".into(),
            ..Default::default()
        };
        let with = |names: &[&str]| {
            let mut s = Snapshot::default();
            s.services.systemd = SystemdState::Ready(Systemd {
                failed: names.iter().map(|n| unit(n)).collect(),
                ..Default::default()
            });
            s.gpus = vec![Gpu {
                sleeping: true,
                temp_c: Some(99.0),
                ..Default::default()
            }];
            s
        };
        let mut e = Engine::new();
        e.evaluate(&with(&["old.service"]), 0.0, 1);
        assert!(kinds(&e).is_empty());
        e.evaluate(&with(&["old.service", "cups.service"]), 1.0, 2);
        let k = kinds(&e);
        assert_eq!(k.len(), 1);
        assert!(k[0].1.contains("cups.service failed (exit code 1)"));
        e.evaluate(&with(&["old.service"]), 2.0, 3);
        assert_eq!(kinds(&e).len(), 2);
    }

    #[test]
    fn busy_process_needs_a_minute() {
        use crate::collect::procs::ProcInfo;
        let s = |cpu: f64| {
            let mut s = Snapshot::default();
            s.procs.list = vec![ProcInfo {
                pid: 7,
                name: "ffmpeg".into(),
                cpu,
                ..Default::default()
            }];
            s
        };
        let mut e = Engine::new();
        for t in 0..60 {
            e.evaluate(&s(99.0), t as f64, t);
        }
        assert!(kinds(&e).is_empty());
        e.evaluate(&s(99.0), 60.0, 60);
        let k = kinds(&e);
        assert_eq!(k.len(), 1);
        assert!(k[0].1.contains("ffmpeg"));
        assert_eq!(e.active_count(), 0, "info alerts are not counted");
    }
}
