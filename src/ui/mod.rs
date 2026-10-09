//! Top-level layout.

pub mod cpu;
pub mod disks;
pub mod footer;
pub mod gpu;
pub mod header;
pub mod help;
pub mod mem;
pub mod net;
pub mod procs;
pub mod sensors;
pub mod services;
pub mod theme;
pub mod widgets;

use crate::app::{App, Panel};
use crate::collect::Snapshot;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::{Block, Paragraph};

pub fn draw(f: &mut Frame, app: &App) {
    let area = f.area();
    // Paint the whole background explicitly.
    f.render_widget(
        Block::default().style(Style::default().bg(theme::BG).fg(theme::TEXT)),
        area,
    );
    if area.width < 40 || area.height < 10 {
        f.render_widget(
            Paragraph::new(Line::styled(
                "Terminal too small (need 40x10)",
                theme::dim(),
            )),
            area,
        );
        return;
    }
    let [hdr, body, ftr] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(area);
    header::draw(f, hdr, app.snap.as_ref(), app.alerts.active_count());
    footer::draw(f, ftr, app);
    let Some(snap) = app.snap.as_ref() else {
        f.render_widget(
            Paragraph::new(Line::styled(
                "  Waiting for the first sample…",
                theme::dim(),
            )),
            body,
        );
        return;
    };

    for (panel, r) in layout(body, app.focus, snap) {
        match panel {
            Panel::Cpu => cpu::draw(f, r, app, snap),
            Panel::Mem => mem::draw(f, r, app, snap),
            Panel::Gpu => gpu::draw(f, r, app, snap),
            Panel::Sensors => sensors::draw(f, r, app, snap),
            Panel::Network => net::draw(f, r, app, snap),
            Panel::Disks => disks::draw(f, r, app, snap),
            Panel::Procs => procs::draw(f, r, app, snap),
            Panel::Services => services::draw(f, r, app, snap),
        }
    }

    if let Some(d) = &app.kill {
        help::draw_kill(f, area, d);
    } else if app.help {
        help::draw_help(f, area);
    }
}

fn split_h(r: Rect, pct: u16) -> (Rect, Rect) {
    let [a, b] = Layout::horizontal([
        Constraint::Percentage(pct),
        Constraint::Percentage(100 - pct),
    ])
    .areas(r);
    (a, b)
}

fn split_v(r: Rect, pct: u16) -> (Rect, Rect) {
    let [a, b] = Layout::vertical([
        Constraint::Percentage(pct),
        Constraint::Percentage(100 - pct),
    ])
    .areas(r);
    (a, b)
}

/// Panel shown next to/below CPU when space is short: the focused one of
/// Memory/GPU/Network/Sensors/Disks/Services.
fn slot(focus: Panel) -> Panel {
    match focus {
        Panel::Gpu | Panel::Sensors | Panel::Network | Panel::Disks | Panel::Services => focus,
        _ => Panel::Mem,
    }
}

/// Where each panel goes. Layout tiers:
/// - 200+ cols: left area (CPU | Memory, Network | Sensors, Processes) plus a right
///   column with GPU, Disks and Services & Alerts (about 40%);
/// - 160 to 199 cols: CPU | Memory, Network | Sensors, GPU | Disks | Services, Processes;
/// - 120 to 159 cols: the same, with Services under Disks (or in a row of its own when the
///   terminal is 48+ rows tall);
/// - narrower: stacked.
pub fn layout(body: Rect, focus: Panel, snap: &Snapshot) -> Vec<(Panel, Rect)> {
    let mut out = Vec::new();
    let h = body.height;
    if body.width < 120 {
        if h >= 50 {
            let ch = [
                Constraint::Percentage(11),
                Constraint::Percentage(8),
                Constraint::Percentage(11),
                Constraint::Percentage(12),
                Constraint::Percentage(9),
                Constraint::Percentage(10),
                Constraint::Percentage(11),
                Constraint::Percentage(28),
            ];
            let r = Layout::vertical(ch).split(body);
            for (p, i) in [
                (Panel::Cpu, 0),
                (Panel::Mem, 1),
                (Panel::Gpu, 2),
                (Panel::Network, 3),
                (Panel::Sensors, 4),
                (Panel::Disks, 5),
                (Panel::Services, 6),
                (Panel::Procs, 7),
            ] {
                out.push((p, r[i]));
            }
        } else {
            let (top, bottom) = split_v(body, 50);
            let (a, b) = split_v(top, 55);
            out.extend([(Panel::Cpu, a), (slot(focus), b), (Panel::Procs, bottom)]);
        }
        return out;
    }
    let wide = body.width >= 200;
    let compact = h < 34 && !wide;
    let (left, right) = if wide {
        let (l, r) = split_h(body, 74);
        (l, Some(r))
    } else {
        (body, None)
    };
    let svc_row = !wide && !compact && body.width < 160 && h >= 48;
    // 120-159 cols, not tall enough for a row of its own: Services shares the right half of
    // the bottom row with Disks, so that row gets the extra height.
    let svc_stack = !wide && !compact && body.width < 160 && !svc_row;
    let upper_pct = if compact {
        55
    } else if wide {
        70
    } else if svc_stack {
        74
    } else {
        68
    };
    let (upper, procs_r) = split_v(left, upper_pct);
    out.push((Panel::Procs, procs_r));
    let bat_lines =
        sensors::battery_lines(snap.battery.as_ref(), snap.power_profile.as_deref()).len();
    let sens_w = (left.width as usize * 48 / 100).saturating_sub(4);
    let want = sensors::rows_needed(snap, sens_w) + bat_lines + 2;
    // Network wants status rows, a graph and the connections list.
    const NET_WANT: usize = 24;
    const CPU_WANT: usize = 22;
    // Rows of the upper area: (CPU | Memory), (Network | Sensors) and, in the medium tier,
    // (GPU | Disks).
    let (row1, row2, row3) = if compact {
        (upper, None, None)
    } else if wide {
        // CPU keeps enough rows for 16 cores in two columns; Network/Sensors get the rest.
        let max = (upper.height as usize)
            .saturating_sub(CPU_WANT)
            .max(upper.height as usize * 40 / 100);
        let h2 = want.max(NET_WANT).min(max) as u16;
        if h2 < 5 {
            (upper, None, None)
        } else {
            let [a, b] =
                Layout::vertical([Constraint::Min(0), Constraint::Length(h2)]).areas(upper);
            (a, Some(b), None)
        }
    } else if svc_row {
        let [a, b, c, d] = Layout::vertical([
            Constraint::Percentage(22),
            Constraint::Percentage(30),
            Constraint::Percentage(26),
            Constraint::Percentage(22),
        ])
        .areas(upper);
        out.push((Panel::Services, d));
        (a, Some(b), Some(c))
    } else {
        let pct = if svc_stack {
            [22, 28, 50]
        } else {
            [28, 36, 36]
        };
        let [a, b, c] = Layout::vertical(pct.map(Constraint::Percentage)).areas(upper);
        (a, Some(b), Some(c))
    };
    let (cpu_r, mem_r) = split_h(row1, 50);
    out.push((Panel::Cpu, cpu_r));
    out.push((if compact { slot(focus) } else { Panel::Mem }, mem_r));
    if let Some(r2) = row2 {
        let (n, s) = split_h(r2, 52);
        out.push((Panel::Network, n));
        out.push((Panel::Sensors, s));
    }
    if let Some(r3) = row3 {
        if svc_row {
            let (g, d) = split_h(r3, 50);
            out.push((Panel::Gpu, g));
            out.push((Panel::Disks, d));
        } else if svc_stack {
            let (g, rest) = split_h(r3, 50);
            let (d, v) = split_v(rest, 55);
            out.push((Panel::Gpu, g));
            out.push((Panel::Disks, d));
            out.push((Panel::Services, v));
        } else {
            let [g, d, v] = Layout::horizontal([
                Constraint::Percentage(34),
                Constraint::Percentage(33),
                Constraint::Percentage(33),
            ])
            .areas(r3);
            out.push((Panel::Gpu, g));
            out.push((Panel::Disks, d));
            out.push((Panel::Services, v));
        }
    }
    if let Some(r) = right {
        let [g, d, v] = Layout::vertical([
            Constraint::Percentage(32),
            Constraint::Percentage(28),
            Constraint::Percentage(40),
        ])
        .areas(r);
        out.push((Panel::Gpu, g));
        out.push((Panel::Disks, d));
        out.push((Panel::Services, v));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collect::cpu::{Core, CpuInfo};
    use crate::collect::disks::{DiskIo, DisksInfo, Mount, Smart, SmartState};
    use crate::collect::gpu::{Gpu, GpuProc, Vendor};
    use crate::collect::mem::{MemInfo, Zram};
    use crate::collect::net::{ConnApp, Gateway, Iface, IfaceKind, NetInfo, Sockets};
    use crate::collect::procs::{ProcInfo, ProcsInfo};
    use crate::collect::sensors::{Battery, Chip as SChip, Fan, Sensors, TempSensor};
    use crate::collect::services::{
        Container, ContainersState, FailedUnit, ServicesInfo, Systemd, SystemdState,
    };
    use crate::collect::{HostInfo, Snapshot};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn synthetic(sleeping_dgpu: bool) -> Snapshot {
        let gib = 1024u64 * 1024 * 1024;
        let list: Vec<ProcInfo> = (1..=40)
            .map(|i| ProcInfo {
                pid: i,
                ppid: if i == 1 { 0 } else { i / 2 },
                user: "tester".into(),
                name: format!("proc{i}"),
                cmd: format!("/usr/bin/proc{i} --flag value"),
                cpu: i as f64 * 2.0,
                mem: i as u64 * 10_000_000,
                read_bps: 1000.0 * i as f64,
                write_bps: 0.0,
                state: 'S',
                threads: 4,
                gpu_pct: (i % 3 == 0).then_some(i as f64),
                gpu_vram: (i % 3 == 0).then_some(i as u64 * 1_000_000),
                start_time: 0,
            })
            .collect();
        Snapshot {
            host: HostInfo {
                hostname: "testhost".into(),
                kernel: "7.0.0".into(),
            },
            cpu: CpuInfo {
                model: "Test CPU".into(),
                physical_cores: 8,
                threads: 16,
                total: 34.2,
                cores: (0..16)
                    .map(|i| Core {
                        usage: i as f64 * 6.0,
                        freq_mhz: Some(3800.0),
                    })
                    .collect(),
                max_mhz: Some(5200.0),
                governor: Some("powersave".into()),
                load: [2.14, 1.82, 1.45],
                uptime_secs: 4 * 86400 + 16 * 3600 + 22 * 60,
            },
            mem: MemInfo {
                total: 31 * gib,
                used: 11 * gib,
                available: 20 * gib,
                cache: 5 * gib,
                shared: gib,
                dirty: 1 << 20,
                slab: gib / 2,
                swap_total: 8 * gib,
                swap_used: gib,
                zram: vec![Zram {
                    name: "zram0".into(),
                    disksize: Some(8 * gib),
                    orig: Some(4 * gib),
                    compr: Some(gib),
                }],
            },
            procs: ProcsInfo {
                total: list.len(),
                running: 2,
                sleeping: 38,
                threads: 160,
                list,
            },
            gpus: vec![
                Gpu {
                    index: 0,
                    name: "NVIDIA GeForce RTX 4050 Laptop".into(),
                    vendor: Vendor::Nvidia,
                    pci: "0000:01:00.0".into(),
                    discrete: true,
                    sleeping: sleeping_dgpu,
                    util: (!sleeping_dgpu).then_some(71.0),
                    vram_used: (!sleeping_dgpu).then_some(2 * gib),
                    vram_total: (!sleeping_dgpu).then_some(6 * gib),
                    temp_c: (!sleeping_dgpu).then_some(71.4),
                    power_w: (!sleeping_dgpu).then_some(31.2),
                    power_cap_w: (!sleeping_dgpu).then_some(60.0),
                    clock_mhz: (!sleeping_dgpu).then_some(1800),
                    mem_clock_mhz: (!sleeping_dgpu).then_some(6001),
                    pstate: (!sleeping_dgpu).then(|| "P2".into()),
                    ..Default::default()
                },
                Gpu {
                    index: 1,
                    name: "AMD Radeon 780M".into(),
                    vendor: Vendor::Amd,
                    pci: "0000:05:00.0".into(),
                    util: Some(23.0),
                    vram_used: Some(300 << 20),
                    vram_total: Some(512 << 20),
                    gtt_used: Some(gib),
                    gtt_total: Some(16 * gib),
                    temp_c: Some(52.0),
                    power_w: Some(6.5),
                    clock_mhz: Some(800),
                    mem_clock_mhz: Some(2400),
                    procs: vec![GpuProc {
                        pid: 3,
                        name: "proc3".into(),
                        gpu_pct: Some(20.0),
                        vram: Some(100 << 20),
                    }],
                    ..Default::default()
                },
            ],
            sensors: Sensors {
                chips: vec![
                    SChip {
                        name: "k10temp".into(),
                        title: "CPU".into(),
                        temps: vec![TempSensor {
                            label: "Tctl".into(),
                            celsius: 84.0,
                            ..Default::default()
                        }],
                        fans: vec![],
                    },
                    SChip {
                        name: "nvme".into(),
                        title: "NVMe SSD".into(),
                        temps: vec![
                            TempSensor {
                                label: "Composite".into(),
                                celsius: 41.0,
                                max: Some(81.85),
                                crit: Some(84.85),
                            },
                            TempSensor {
                                label: "Sensor 1".into(),
                                celsius: 43.0,
                                ..Default::default()
                            },
                        ],
                        fans: vec![],
                    },
                    SChip {
                        name: "asus".into(),
                        title: "ASUS fans".into(),
                        temps: vec![],
                        fans: vec![Fan {
                            label: "cpu_fan".into(),
                            rpm: 3420,
                        }],
                    },
                ],
                cpu_temp: Some(84.0),
                cpu_power_w: None,
            },
            battery: Some(Battery {
                capacity: 92.0,
                status: "Discharging".into(),
                rate_w: Some(14.2),
                energy_now: Some(45.0),
                energy_full: Some(48.0),
                energy_design: Some(50.0),
                cycles: Some(42),
                time_left_secs: Some(3 * 3600 + 12 * 60),
                ac_online: Some(false),
            }),
            power_profile: Some("performance".into()),
            net: synthetic_net(),
            disks: synthetic_disks(),
            services: synthetic_services(),
        }
    }

    fn synthetic_services() -> ServicesInfo {
        ServicesInfo {
            systemd: SystemdState::Ready(Systemd {
                running: 142,
                total: 147,
                failed: vec![
                    FailedUnit {
                        name: "cups.service".into(),
                        description: "CUPS Scheduler".into(),
                        reason: "exit code 1".into(),
                        since: "14:28".into(),
                        user: false,
                    },
                    FailedUnit {
                        name: "mstop.service".into(),
                        reason: "timed out".into(),
                        since: "09:05".into(),
                        user: true,
                        ..Default::default()
                    },
                ],
                timers: 3,
                next_timer: Some((
                    "backup.timer".into(),
                    crate::collect::services::unix_now() + 12 * 60 + 30,
                )),
            }),
            containers: ContainersState::NotInstalled,
        }
    }

    fn synthetic_net() -> NetInfo {
        let gib = 1024u64 * 1024 * 1024;
        NetInfo {
            ifaces: vec![
                Iface {
                    name: "wlan0".into(),
                    kind: IfaceKind::Wifi,
                    up: true,
                    operstate: "up".into(),
                    speed_mbps: Some(1201),
                    rx_bps: 14_200_000.0,
                    tx_bps: 1_800_000.0,
                    rx_total: 42 * gib,
                    tx_total: 11 * gib,
                    signal_dbm: Some(-44),
                    ipv4: vec!["192.168.1.142/24".into()],
                    ..Default::default()
                },
                // Unplugged ethernet port.
                Iface {
                    name: "eno1".into(),
                    kind: IfaceKind::Ethernet,
                    up: false,
                    operstate: "down".into(),
                    ..Default::default()
                },
            ],
            gateway: Some(Gateway {
                iface: "wlan0".into(),
                ip: "192.168.1.1".into(),
            }),
            sockets: Sockets {
                established: 12,
                listen: 9,
                time_wait: 20,
                other: 7,
                udp: 18,
            },
            apps: vec![
                ConnApp {
                    name: "firefox".into(),
                    established: 34,
                    listening: vec![],
                },
                ConnApp {
                    name: "sshd".into(),
                    established: 1,
                    listening: vec![22],
                },
            ],
            rx_bps: 14_200_000.0,
            tx_bps: 1_800_000.0,
        }
    }

    fn synthetic_disks() -> DisksInfo {
        let gib = 1024u64 * 1024 * 1024;
        DisksInfo {
            disks: vec![
                DiskIo {
                    name: "nvme0n1".into(),
                    model: Some("Samsung SSD 990 PRO".into()),
                    size: 953 * gib,
                    read_bps: 40_000_000.0,
                    write_bps: 3_000_000.0,
                    read_iops: 2420.0,
                    write_iops: 840.0,
                    read_lat_ms: Some(0.3),
                    write_lat_ms: Some(1.1),
                    busy_pct: 12.0,
                    smart: SmartState::Ready(Smart {
                        passed: Some(true),
                        temp_c: Some(41.0),
                        power_on_hours: Some(1234),
                        pct_used: Some(3.0),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                DiskIo {
                    name: "sda".into(),
                    model: Some("WD Blue".into()),
                    size: 1000 * gib,
                    rotational: true,
                    smart: SmartState::NeedsRoot,
                    ..Default::default()
                },
            ],
            mounts: vec![
                Mount {
                    points: vec!["/".into(), "/home".into()],
                    source: "/dev/nvme0n1p2".into(),
                    fstype: "btrfs".into(),
                    disk: "nvme0n1".into(),
                    total: 953 * gib,
                    used: 412 * gib,
                    avail: 541 * gib,
                },
                Mount {
                    points: vec!["/mnt/data".into()],
                    source: "/dev/sda1".into(),
                    fstype: "ext4".into(),
                    disk: "sda".into(),
                    total: 1000 * gib,
                    used: 910 * gib,
                    avail: 90 * gib,
                },
            ],
        }
    }

    fn render(w: u16, h: u16, tweak: impl Fn(&mut App)) -> String {
        render_with(w, h, false, tweak)
    }

    fn render_with(w: u16, h: u16, sleeping: bool, tweak: impl Fn(&mut App)) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        let mut app = App::new(1000);
        for _ in 0..5 {
            app.apply_snapshot(synthetic(sleeping));
        }
        tweak(&mut app);
        term.draw(|f| draw(f, &app)).unwrap();
        let buf = term.backend().buffer().clone();
        (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn renders_large() {
        let s = render(224, 56, |_| {});
        if std::env::var_os("MSTOP_PRINT").is_some() {
            println!("{s}");
            for (w, h) in [(160, 45), (120, 40), (80, 24)] {
                println!("{}", render(w, h, |_| {}));
            }
        }
        for needle in [
            "SYS: OK",
            "GPU 0: 71°C",
            "BAT: 92%",
            "FAN: 3,420 RPM",
            "PROFILE: performance",
            "Apps using this GPU:",
            "Video memory used",
            "of 60 W limit",
            "Package temperature",
            "needs root",
            "Battery: 92%",
            "3 h 12 min left",
            "health 96%",
            "42 cycles",
            "Selected PID",
            "GPU%",
            "Memory used currently",
            "Load average (1m, 5m, 15m)",
            "4 days, 16 hours, 22 minutes",
            "NAME/COMMAND",
            "ZRAM",
            "now",
            "[F1] Help",
            "C16",
            "4 Network · wlan0",
            "↓ 14.2 MB/s · ↑ 1.8 MB/s",
            "Primary (wlan0):",
            "Wi-Fi · 1,201 Mbps · signal -44 dBm (good) · 192.168.1.142/24",
            "Gateway:",
            "Disconnected (cable unplugged)",
            "↓ 42.0 GiB · ↑ 11.0 GiB · 0 errors",
            "Connections:",
            "UDP 18",
            "Most connections (per-app traffic needs root)",
            "firefox",
            "6 Disks",
            "R 40.0 MB/s · W 3.0 MB/s",
            "/, /home",
            "nvme0n1 · btrfs",
            "412 GiB of 953 GiB used (43%)",
            "✓ SMART OK",
            "R 40.0 MB/s (2,420 IOPS, 0.3 ms)",
            "busy 12%",
            "41 °C",
            "SMART: run once as root or add a udev rule",
            "NET: 14.2 MB/s",
            "SMART: OK",
            "IO: 43.0 MB/s",
            "DISK: /mnt/data 91% full",
        ] {
            assert!(s.contains(needle), "missing {needle:?}");
        }
    }

    #[test]
    fn io_panels_all_sizes() {
        for (w, h, needles) in [
            (224u16, 56u16, &["4 Network", "6 Disks"][..]),
            (160, 45, &["4 Network", "6 Disks", "Sensors"][..]),
            (120, 40, &["4 Network", "6 Disks"][..]),
            (80, 24, &["CPU usage now"][..]),
        ] {
            let s = render(w, h, |_| {});
            for n in needles {
                assert!(s.contains(n), "{w}x{h} missing {n:?}");
            }
        }
        // Focusing the I/O panels swaps them into the compact slot.
        for p in [Panel::Network, Panel::Disks] {
            let s = render(80, 24, |a| a.focus = p);
            assert!(s.contains(if p == Panel::Network {
                "Network"
            } else {
                "Disks"
            }));
            for (w, h) in [(120, 30), (60, 12), (100, 60), (210, 25)] {
                render(w, h, |a| a.focus = p);
            }
        }
        // Failing SMART, a full disk and an idle system.
        let s = render(224, 56, |a| {
            let snap = a.snap.as_mut().unwrap();
            if let SmartState::Ready(m) = &mut snap.disks.disks[0].smart {
                m.passed = Some(false);
            }
            snap.disks.mounts[1].used = 960 * 1024 * 1024 * 1024;
            snap.disks.mounts[1].avail = 40 * 1024 * 1024 * 1024;
            snap.net.ifaces.clear();
            snap.net.gateway = None;
            snap.net.apps.clear();
        });
        assert!(s.contains("SMART: nvme0n1 failing"));
        assert!(s.contains("✖ SMART failing"));
        assert!(s.contains("DISK: /mnt/data 96% full"));
        assert!(s.contains("No active connection"));
        let s = render(120, 40, |a| {
            let snap = a.snap.as_mut().unwrap();
            snap.disks = DisksInfo::default();
        });
        assert!(s.contains("No disks detected."));
    }

    fn hot_gpu(a: &mut App) {
        let mut snap = a.snap.clone().unwrap();
        snap.gpus[0].temp_c = Some(86.0);
        let wall = crate::collect::services::unix_now();
        a.alerts.evaluate(&snap, 100.0, wall);
        a.alerts.evaluate(&snap, 111.0, wall);
        a.snap = Some(snap);
    }

    #[test]
    fn services_and_alerts_panel() {
        let s = render(224, 56, hot_gpu);
        if std::env::var_os("MSTOP_PRINT").is_some() {
            println!("{s}");
        }
        for needle in [
            "8 Services & Alerts",
            "Services: 142 running · 2 failed · 3 timers",
            "next timer: backup.timer in 12 min",
            "✖ cups.service — failed (exit code 1) · 14:28",
            "✖ mstop.service (user) — failed (timed out) · 09:05",
            "Docker: not installed",
            "▲ GPU 0 temperature 86 °C (above 85 °C for 10 s)",
            "SVCS: 2 failed",
            "ALERTS: ",
            "GPU 0: 86°C",
        ] {
            assert!(s.contains(needle), "missing {needle:?}");
        }
        // GPU 0 (discrete) is listed before GPU 1.
        let nv = s.find("GPU 0 · NVIDIA").expect("discrete GPU numbered 0");
        let amd = s.find("GPU 1 · AMD").expect("integrated GPU numbered 1");
        assert!(nv < amd);
        // Clean system: no failed units, no chip, quiet alert log.
        let s = render(224, 56, |a| {
            let snap = a.snap.as_mut().unwrap();
            snap.services.systemd = SystemdState::Ready(Systemd {
                running: 10,
                total: 12,
                ..Default::default()
            });
            snap.disks.mounts.clear();
            a.alerts = crate::alerts::Engine::new();
        });
        assert!(!s.contains("SVCS:"));
        assert!(s.contains("0 failed"));
        assert!(s.contains("No alerts yet"));
    }

    #[test]
    fn services_panel_all_sizes_and_states() {
        for (w, h, needle) in [
            (224u16, 56u16, "Services: 142 running"),
            (160, 45, "Services: 142 running"),
            (120, 40, "Services: 142 running"),
            (120, 56, "Services: 142 running"),
            (100, 60, "Services: 142 running"),
        ] {
            let s = render(w, h, hot_gpu);
            assert!(s.contains(needle), "{w}x{h} missing services");
            assert!(s.contains("Services & Alerts"), "{w}x{h}");
        }
        let s = render(80, 24, |a| a.focus = Panel::Services);
        assert!(s.contains("Services & Alerts"));
        for (w, h) in [
            (80, 24),
            (60, 12),
            (40, 10),
            (210, 25),
            (130, 20),
            (200, 34),
        ] {
            render(w, h, |a| {
                a.focus = Panel::Services;
                hot_gpu(a);
            });
        }
        let states = [
            SystemdState::Pending,
            SystemdState::Unavailable,
            SystemdState::Ready(Systemd::default()),
        ];
        for st in states {
            let s = render(120, 40, |a| {
                a.snap.as_mut().unwrap().services.systemd = st.clone()
            });
            assert!(s.contains("Services"));
        }
        let s = render(224, 56, |a| {
            a.snap.as_mut().unwrap().services.containers = ContainersState::NoAccess;
        });
        assert!(s.contains("add yourself to the docker group"));
        let s = render(224, 56, |a| {
            a.snap.as_mut().unwrap().services.containers = ContainersState::Ready {
                engine: "Docker",
                list: vec![
                    Container {
                        name: "web".into(),
                        image: "nginx".into(),
                        state: "running".into(),
                        status: "Up 2 hours".into(),
                        cpu_pct: Some(2.5),
                        mem: Some(120 << 20),
                    },
                    Container {
                        name: "db".into(),
                        image: "postgres".into(),
                        state: "exited".into(),
                        status: "Exited (0)".into(),
                        ..Default::default()
                    },
                ],
            };
        });
        assert!(s.contains("Docker: 1 running · 1 stopped"));
    }

    #[test]
    fn summary_is_one_line_when_it_fits() {
        let snap = synthetic(false);
        let lines = services::summary_lines(&snap.services.systemd, 120);
        assert_eq!(lines.len(), 1);
        let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(
            text,
            "Services: 142 running · 2 failed · 3 timers (next: backup.timer in 12 min)"
        );
    }

    #[test]
    fn history_span_labels() {
        let s = render(224, 56, |a| a.span = crate::history::Span::H24);
        for n in [
            "24h ago",
            "18h",
            "12h",
            "6h",
            "span 24 h",
            "[h] History span",
        ] {
            assert!(s.contains(n), "missing {n:?}");
        }
        let s = render(224, 56, |a| a.span = crate::history::Span::H1);
        assert!(s.contains("1h ago") && s.contains("span 1 h"));
        let s = render(224, 56, |a| a.span = crate::history::Span::M10);
        assert!(s.contains("10m ago"));
        let s = render(120, 40, |_| {});
        assert!(s.contains("span 60 s"));
        let s = render(120, 40, |a| a.help = true);
        assert!(s.contains("History span of all graphs"));
    }

    #[test]
    fn polish_cpu_and_sensors() {
        let s = render(224, 56, |_| {});
        // CPU keeps a history graph next to the per-core grid.
        assert!(s.contains("C16"));
        assert!(s.contains("CPU temperature history"));
        for n in ["Fans", "  cpu_fan", "NVMe SSD"] {
            assert!(s.contains(n), "missing {n:?}");
        }
        let lines: Vec<&str> = s.lines().collect();
        let cpu_top = lines.iter().position(|l| l.contains("1 CPU")).unwrap();
        let cpu_graph = lines.iter().position(|l| l.contains("100%")).unwrap();
        assert!(cpu_graph > cpu_top, "CPU graph present");
    }

    #[test]
    fn renders_sleeping_gpu_all_sizes() {
        for (w, h) in [
            (224, 56),
            (120, 40),
            (80, 24),
            (100, 60),
            (150, 30),
            (210, 25),
            (60, 12),
        ] {
            let s = render_with(w, h, true, |_| {});
            if w >= 120 && h >= 40 {
                assert!(s.contains("Sleeping (power saving)"), "{w}x{h}");
                assert!(s.contains("GPU 0: asleep"), "{w}x{h}");
            }
        }
        for p in [Panel::Gpu, Panel::Sensors, Panel::Mem] {
            for (w, h) in [(80, 24), (120, 30)] {
                render_with(w, h, false, |a| a.focus = p);
            }
        }
        let s = render_with(80, 24, false, |a| a.focus = Panel::Gpu);
        assert!(s.contains("Apps using") || s.contains("GPU 0"));
    }

    #[test]
    fn renders_medium() {
        let s = render(120, 40, |_| {});
        for n in ["Sensors", "GPU", "Memory used currently", "Processes"] {
            assert!(s.contains(n), "missing {n:?}");
        }
    }

    #[test]
    fn renders_small_and_tiny() {
        let s = render(80, 24, |_| {});
        assert!(s.contains("CPU usage now"));
        assert!(s.contains("Processes"));
        for (w, h) in [(40, 10), (30, 8), (100, 30), (224, 20), (120, 56)] {
            render(w, h, |_| {});
        }
    }

    #[test]
    fn renders_overlays_and_modes() {
        let s = render(120, 40, |a| {
            a.help = true;
        });
        assert!(s.contains("Toggle this help"));
        let s = render(120, 40, |a| {
            a.tree = true;
            a.filter = "proc1".into();
            a.rebuild_rows();
        });
        assert!(s.contains("matches shown of 40"));
        let s = render(120, 40, |a| {
            a.kill = Some(crate::app::KillDialog {
                pid: 3,
                name: "proc3".into(),
                start_ticks: 0,
            });
        });
        assert!(s.contains("Send SIGTERM to proc3 (PID 3)?"));
    }
}
