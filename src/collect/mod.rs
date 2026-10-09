//! Collector thread: produces a `Snapshot` every interval.

pub mod cpu;
pub mod disks;
pub mod gpu;
pub mod mem;
pub mod net;
pub mod procs;
pub mod sensors;
pub mod services;

use crate::fmt;
use std::fs;
use std::time::{Duration, Instant};
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

#[derive(Debug, Clone, Default)]
pub struct HostInfo {
    pub hostname: String,
    pub kernel: String,
}

#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub host: HostInfo,
    pub cpu: cpu::CpuInfo,
    pub mem: mem::MemInfo,
    pub procs: procs::ProcsInfo,
    pub gpus: Vec<gpu::Gpu>,
    pub sensors: sensors::Sensors,
    pub battery: Option<sensors::Battery>,
    pub power_profile: Option<String>,
    pub net: net::NetInfo,
    pub disks: disks::DisksInfo,
    pub services: services::ServicesInfo,
}

pub struct Collector {
    sys: System,
    cpu: cpu::CpuSampler,
    mem: mem::MemSampler,
    procs: procs::ProcSampler,
    gpu: gpu::GpuSampler,
    sensors: sensors::SensorSampler,
    net: net::NetSampler,
    disks: disks::DiskSampler,
    services: services::ServicesSampler,
    host: HostInfo,
    last: Instant,
}

fn proc_kind() -> ProcessRefreshKind {
    ProcessRefreshKind::nothing()
        .with_cpu()
        .with_memory()
        .with_disk_usage()
        .with_user(UpdateKind::OnlyIfNotSet)
        .with_cmd(UpdateKind::OnlyIfNotSet)
        .without_tasks()
}

impl Collector {
    pub fn new() -> Self {
        let mut sys = System::new();
        sys.refresh_cpu_all();
        sys.refresh_processes_specifics(ProcessesToUpdate::All, true, proc_kind());
        let cpu = cpu::CpuSampler::new(&sys);
        let host = HostInfo {
            hostname: System::host_name().unwrap_or_else(|| "localhost".into()),
            kernel: fs::read_to_string("/proc/sys/kernel/osrelease")
                .map(|s| s.trim().to_string())
                .unwrap_or_default(),
        };
        Self {
            sys,
            cpu,
            mem: mem::MemSampler,
            procs: procs::ProcSampler::new(),
            gpu: gpu::GpuSampler::new(),
            sensors: sensors::SensorSampler::new(),
            net: net::NetSampler::new(),
            disks: disks::DiskSampler::new(),
            services: services::ServicesSampler::new(),
            host,
            last: Instant::now(),
        }
    }

    pub fn sample(&mut self) -> Snapshot {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last).as_secs_f64();
        self.last = now;
        self.sys.refresh_cpu_all();
        self.sys
            .refresh_processes_specifics(ProcessesToUpdate::All, true, proc_kind());
        let mut procs = self.procs.sample(&self.sys, elapsed);
        let mut gpus = self.gpu.sample();
        // Number GPUs in display order (discrete first) so every view agrees.
        gpus.sort_by_key(|g| (!g.discrete, g.index));
        for (i, g) in gpus.iter_mut().enumerate() {
            g.index = i;
        }
        // Attach per-process GPU usage to the process table.
        let mut by_pid: std::collections::HashMap<u32, (Option<f64>, u64)> =
            std::collections::HashMap::new();
        for g in &gpus {
            for p in &g.procs {
                let e = by_pid.entry(p.pid).or_default();
                if let Some(pct) = p.gpu_pct {
                    e.0 = Some(e.0.unwrap_or(0.0) + pct);
                }
                e.1 += p.vram.unwrap_or(0);
            }
        }
        for p in &mut procs.list {
            if let Some((pct, vram)) = by_pid.get(&p.pid) {
                p.gpu_pct = *pct;
                p.gpu_vram = Some(*vram);
            }
        }
        let (sensors, battery, power_profile) = self.sensors.sample();
        Snapshot {
            host: self.host.clone(),
            cpu: self.cpu.sample(&self.sys),
            mem: self.mem.sample(),
            procs,
            gpus,
            sensors,
            battery,
            power_profile,
            net: self.net.sample(),
            disks: self.disks.sample(),
            services: self.services.sample(),
        }
    }
}

/// Run the collector on its own thread. `send` returns false when the receiver is gone.
pub fn spawn<F>(interval: Duration, send: F)
where
    F: Fn(Snapshot) -> bool + Send + 'static,
{
    std::thread::spawn(move || {
        let mut c = Collector::new();
        // sysinfo needs a short gap between two CPU refreshes for meaningful numbers.
        std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL.max(Duration::from_millis(250)));
        loop {
            let started = Instant::now();
            if !send(c.sample()) {
                break;
            }
            std::thread::sleep(interval.saturating_sub(started.elapsed()));
        }
    });
}

impl Snapshot {
    /// Plain-text dump of every collected value (used by `--dump`).
    pub fn dump(&self) -> String {
        use std::fmt::Write;
        let mut s = String::new();
        let c = &self.cpu;
        let _ = writeln!(
            s,
            "host: {} (kernel {})",
            self.host.hostname, self.host.kernel
        );
        let _ = writeln!(s, "cpu.model: {}", c.model);
        let _ = writeln!(s, "cpu.cores/threads: {}C/{}T", c.physical_cores, c.threads);
        let _ = writeln!(s, "cpu.total: {}", fmt::pct(c.total));
        let _ = writeln!(
            s,
            "cpu.governor: {}",
            c.governor.as_deref().unwrap_or("n/a")
        );
        let _ = writeln!(
            s,
            "cpu.max_boost: {}",
            c.max_mhz.map(fmt::ghz).unwrap_or_else(|| "n/a".into())
        );
        let _ = writeln!(
            s,
            "cpu.load: {:.2} {:.2} {:.2}",
            c.load[0], c.load[1], c.load[2]
        );
        let _ = writeln!(s, "cpu.uptime: {}", fmt::duration(c.uptime_secs));
        for (i, core) in c.cores.iter().enumerate() {
            let f = core.freq_mhz.map(fmt::ghz).unwrap_or_else(|| "n/a".into());
            let _ = writeln!(s, "cpu.core{:02}: {} @ {}", i + 1, fmt::pct(core.usage), f);
        }
        let m = &self.mem;
        let _ = writeln!(s, "mem.total: {}", fmt::bytes(m.total));
        let _ = writeln!(
            s,
            "mem.used: {}  available: {}",
            fmt::bytes(m.used),
            fmt::bytes(m.available)
        );
        let _ = writeln!(s, "mem.cache+buffers+sreclaimable: {}", fmt::bytes(m.cache));
        let _ = writeln!(
            s,
            "mem.shared: {}  dirty: {}  slab: {}",
            fmt::bytes(m.shared),
            fmt::bytes(m.dirty),
            fmt::bytes(m.slab)
        );
        let _ = writeln!(
            s,
            "swap: {} used of {}",
            fmt::bytes(m.swap_used),
            fmt::bytes(m.swap_total)
        );
        for z in &m.zram {
            let _ = writeln!(
                s,
                "zram.{}: disksize {} orig {} compr {} ratio {} saved {}",
                z.name,
                z.disksize.map(fmt::bytes).unwrap_or_else(|| "n/a".into()),
                z.orig.map(fmt::bytes).unwrap_or_else(|| "n/a".into()),
                z.compr.map(fmt::bytes).unwrap_or_else(|| "n/a".into()),
                z.ratio()
                    .map(|r| format!("{r:.1}:1"))
                    .unwrap_or_else(|| "n/a".into()),
                z.saved().map(fmt::bytes).unwrap_or_else(|| "n/a".into()),
            );
        }
        let p = &self.procs;
        let _ = writeln!(
            s,
            "procs: {} total, {} running, {} sleeping, {} threads",
            p.total, p.running, p.sleeping, p.threads
        );
        let mut top: Vec<_> = p.list.iter().collect();
        top.sort_by(|a, b| b.cpu.total_cmp(&a.cpu));
        for q in top.iter().take(8) {
            let cmd: String = q.cmd.chars().take(60).collect();
            let _ = writeln!(
                s,
                "proc {:>7} ppid {:>7} {:<10} {} cpu {:.1}% mem {} r {} w {} thr {} start {} :: {}",
                q.pid,
                q.ppid,
                q.user,
                q.state,
                q.cpu,
                fmt::bytes(q.mem),
                fmt::rate(q.read_bps),
                fmt::rate(q.write_bps),
                q.threads,
                q.start_time,
                cmd
            );
        }
        self.dump_hw(&mut s);
        self.dump_io(&mut s);
        self.dump_services(&mut s);
        s
    }

    fn dump_hw(&self, s: &mut String) {
        use std::fmt::Write;
        let na = || "n/a".to_string();
        let f1 = |v: Option<f64>, u: &str| v.map(|v| format!("{v:.1}{u}")).unwrap_or_else(na);
        for g in &self.gpus {
            let kind = if g.discrete { "discrete" } else { "integrated" };
            let _ = writeln!(
                s,
                "gpu{}: {} [{}] {} {}",
                g.index,
                g.name,
                g.pci,
                kind,
                g.vendor.label()
            );
            if g.sleeping {
                let _ = writeln!(
                    s,
                    "gpu{}.state: sleeping (runtime suspended, not queried)",
                    g.index
                );
                continue;
            }
            let b = |v: Option<u64>| v.map(fmt::bytes).unwrap_or_else(na);
            let _ = writeln!(
                s,
                "gpu{}.util: {}  vram: {} of {}  gtt: {} of {}",
                g.index,
                f1(g.util, "%"),
                b(g.vram_used),
                b(g.vram_total),
                b(g.gtt_used),
                b(g.gtt_total)
            );
            let _ = writeln!(
                s,
                "gpu{}.temp: {}  power: {} of {}  fan: {} / {}  clock: {} MHz  mem clock: {} MHz  pstate: {}",
                g.index,
                f1(g.temp_c, " C"),
                f1(g.power_w, " W"),
                f1(g.power_cap_w, " W"),
                f1(g.fan_pct, "%"),
                g.fan_rpm.map(|r| format!("{r} RPM")).unwrap_or_else(na),
                g.clock_mhz.map(|c| c.to_string()).unwrap_or_else(na),
                g.mem_clock_mhz.map(|c| c.to_string()).unwrap_or_else(na),
                g.pstate.as_deref().unwrap_or("n/a")
            );
            for p in g.procs.iter().take(5) {
                let _ = writeln!(
                    s,
                    "gpu{}.proc {:>7} {:<16} gpu {} vram {}",
                    g.index,
                    p.pid,
                    p.name,
                    f1(p.gpu_pct, "%"),
                    b(p.vram)
                );
            }
        }
        let se = &self.sensors;
        let _ = writeln!(s, "sensors.cpu_temp: {}", f1(se.cpu_temp, " C"));
        let _ = writeln!(
            s,
            "sensors.cpu_power: {}",
            se.cpu_power_w
                .map(|w| format!("{w:.1} W"))
                .unwrap_or_else(|| "needs root (or amd_energy)".into())
        );
        for c in &se.chips {
            for t in &c.temps {
                let _ = writeln!(
                    s,
                    "sensor {} ({}): {} {:.1} C max {} crit {}",
                    c.title,
                    c.name,
                    t.label,
                    t.celsius,
                    f1(t.max, ""),
                    f1(t.crit, "")
                );
            }
            for fan in &c.fans {
                let _ = writeln!(
                    s,
                    "fan {} ({}): {} {} RPM",
                    c.title, c.name, fan.label, fan.rpm
                );
            }
        }
        match &self.battery {
            Some(bat) => {
                let _ = writeln!(
                    s,
                    "battery: {:.0}% {} rate {} left {} health {} cycles {} ac {}",
                    bat.capacity,
                    bat.status,
                    f1(bat.rate_w, " W"),
                    bat.time_left_secs.map(fmt::duration).unwrap_or_else(na),
                    f1(bat.health_pct(), "%"),
                    bat.cycles.map(|c| c.to_string()).unwrap_or_else(na),
                    bat.ac_online.map(|a| a.to_string()).unwrap_or_else(na)
                );
                let _ = writeln!(
                    s,
                    "battery.energy: now {} full {} design {}",
                    f1(bat.energy_now, " Wh"),
                    f1(bat.energy_full, " Wh"),
                    f1(bat.energy_design, " Wh")
                );
            }
            None => {
                let _ = writeln!(s, "battery: none");
            }
        }
        let _ = writeln!(
            s,
            "power_profile: {}",
            self.power_profile.as_deref().unwrap_or("n/a")
        );
    }

    fn dump_services(&self, s: &mut String) {
        use services::{ContainersState as C, SystemdState as S};
        use std::fmt::Write;
        match &self.services.systemd {
            S::Pending => {
                let _ = writeln!(s, "services: pending");
            }
            S::Unavailable => {
                let _ = writeln!(s, "services: systemd unavailable");
            }
            S::Ready(sd) => {
                let _ = writeln!(
                    s,
                    "services: {} running of {} loaded, {} failed, {} timers (next: {})",
                    sd.running,
                    sd.total,
                    sd.failed.len(),
                    sd.timers,
                    services::next_timer_text(sd)
                );
                for u in &sd.failed {
                    let _ = writeln!(
                        s,
                        "service.failed {}{}: {} since {} :: {}",
                        u.name,
                        if u.user { " (user)" } else { "" },
                        if u.reason.is_empty() {
                            "failed".to_string()
                        } else {
                            format!("failed ({})", u.reason)
                        },
                        if u.since.is_empty() { "?" } else { &u.since },
                        u.description
                    );
                }
            }
        }
        match &self.services.containers {
            C::Pending => {
                let _ = writeln!(s, "containers: pending");
            }
            C::NotInstalled => {
                let _ = writeln!(s, "containers: not installed (no Docker/Podman socket)");
            }
            C::NoAccess => {
                let _ = writeln!(s, "containers: no access ({})", services::NO_ACCESS_HINT);
            }
            C::Error(e) => {
                let _ = writeln!(s, "containers: error {e}");
            }
            C::Ready { engine, list } => {
                let _ = writeln!(s, "containers: {engine}, {} listed", list.len());
                for c in list {
                    let _ = writeln!(
                        s,
                        "container {} ({}): {} / {} cpu {} mem {}",
                        c.name,
                        c.image,
                        c.state,
                        c.status,
                        c.cpu_pct
                            .map(|v| format!("{v:.1}%"))
                            .unwrap_or_else(|| "n/a".into()),
                        c.mem.map(fmt::bytes).unwrap_or_else(|| "n/a".into())
                    );
                }
            }
        }
    }

    fn dump_io(&self, s: &mut String) {
        use std::fmt::Write;
        let ms = |v: Option<f64>| {
            v.map(|v| format!("{v:.2} ms"))
                .unwrap_or_else(|| "n/a".into())
        };
        for d in &self.disks.disks {
            let _ = writeln!(
                s,
                "disk {} ({}, {}, {}): R {} ({:.0} IOPS, {})  W {} ({:.0} IOPS, {})  busy {:.0}%  inflight {}",
                d.name,
                d.model.as_deref().unwrap_or("unknown model"),
                fmt::bytes(d.size),
                d.kind(),
                fmt::rate(d.read_bps),
                d.read_iops,
                ms(d.read_lat_ms),
                fmt::rate(d.write_bps),
                d.write_iops,
                ms(d.write_lat_ms),
                d.busy_pct,
                d.inflight
            );
            let _ = match &d.smart {
                disks::SmartState::Pending => writeln!(s, "smart {}: pending", d.name),
                disks::SmartState::NeedsRoot => {
                    writeln!(
                        s,
                        "smart {}: needs root (run once as root or add a udev rule)",
                        d.name
                    )
                }
                disks::SmartState::Unavailable => writeln!(s, "smart {}: unavailable", d.name),
                disks::SmartState::Ready(m) => writeln!(
                    s,
                    "smart {}: passed {:?} temp {:?} C power-on {:?} h used {:?}% spare {:?}% media errors {:?} unsafe shutdowns {:?}",
                    d.name,
                    m.passed,
                    m.temp_c,
                    m.power_on_hours,
                    m.pct_used,
                    m.avail_spare,
                    m.media_errors,
                    m.unsafe_shutdowns
                ),
            };
        }
        for m in &self.disks.mounts {
            let _ = writeln!(
                s,
                "mount {} on {} ({}, {}): {} used of {} ({:.0}%), {} free",
                m.label(),
                if m.disk.is_empty() { "?" } else { &m.disk },
                m.fstype,
                m.source,
                fmt::bytes(m.used),
                fmt::bytes(m.total),
                m.used_pct(),
                fmt::bytes(m.avail)
            );
        }
        let n = &self.net;
        let _ = writeln!(
            s,
            "net.total: down {} up {}  gateway {}",
            fmt::rate(n.rx_bps),
            fmt::rate(n.tx_bps),
            n.gateway
                .as_ref()
                .map(|g| format!("{} via {}", g.ip, g.iface))
                .unwrap_or_else(|| "none".into())
        );
        for i in &n.ifaces {
            let _ = writeln!(
                s,
                "net {} ({}, {}): down {} up {} total down {} up {} errors {} drops {} speed {} signal {} mac {} v4 {:?} v6 {:?}",
                i.name,
                i.kind.label(),
                i.operstate,
                fmt::rate(i.rx_bps),
                fmt::rate(i.tx_bps),
                fmt::bytes(i.rx_total),
                fmt::bytes(i.tx_total),
                i.errors,
                i.drops,
                i.speed_mbps
                    .map(|v| format!("{v} Mbps"))
                    .unwrap_or_else(|| "n/a".into()),
                i.signal_dbm
                    .map(|v| format!("{v} dBm"))
                    .unwrap_or_else(|| "n/a".into()),
                i.mac.as_deref().unwrap_or("n/a"),
                i.ipv4,
                i.ipv6
            );
        }
        let k = &n.sockets;
        let _ = writeln!(
            s,
            "sockets: tcp {} (established {}, listen {}, time_wait {}, other {}) udp {}",
            k.tcp_total(),
            k.established,
            k.listen,
            k.time_wait,
            k.other,
            k.udp
        );
        for a in &n.apps {
            let _ = writeln!(
                s,
                "conn.app {:<16} established {} listening {:?}",
                a.name, a.established, a.listening
            );
        }
    }
}
