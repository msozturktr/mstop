//! Network sampler: interface rates from /proc/net/dev, socket counts from /proc/net/{tcp,udp},
//! addresses via getifaddrs and a root-free "apps with most connections" list.

use std::collections::HashMap;
use std::fs;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::path::Path;
use std::time::{Duration, Instant};

const SLOW_EVERY: Duration = Duration::from_secs(10);
const SOCKETS_EVERY: Duration = Duration::from_secs(3);
const APPS_EVERY: Duration = Duration::from_secs(5);
const APPS_FULL_EVERY: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IfaceKind {
    Wifi,
    #[default]
    Ethernet,
    Virtual,
    Tunnel,
}

impl IfaceKind {
    pub fn label(self) -> &'static str {
        match self {
            IfaceKind::Wifi => "Wi-Fi",
            IfaceKind::Ethernet => "Ethernet",
            IfaceKind::Virtual => "Virtual",
            IfaceKind::Tunnel => "Tunnel",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Iface {
    pub name: String,
    pub kind: IfaceKind,
    pub up: bool,
    pub operstate: String,
    pub speed_mbps: Option<u32>,
    pub rx_bps: f64,
    pub tx_bps: f64,
    pub rx_total: u64,
    pub tx_total: u64,
    pub errors: u64,
    pub drops: u64,
    pub signal_dbm: Option<i32>,
    pub mac: Option<String>,
    /// `192.168.1.142/24`
    pub ipv4: Vec<String>,
    pub ipv6: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Sockets {
    pub established: u32,
    pub listen: u32,
    pub time_wait: u32,
    pub other: u32,
    pub udp: u32,
}

impl Sockets {
    pub fn tcp_total(&self) -> u32 {
        self.established + self.listen + self.time_wait + self.other
    }
}

#[derive(Debug, Clone, Default)]
pub struct ConnApp {
    pub name: String,
    pub established: u32,
    pub listening: Vec<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gateway {
    pub iface: String,
    pub ip: String,
}

#[derive(Debug, Clone, Default)]
pub struct NetInfo {
    /// Primary (default route) interface first, then by name.
    pub ifaces: Vec<Iface>,
    pub gateway: Option<Gateway>,
    pub sockets: Sockets,
    pub apps: Vec<ConnApp>,
    /// Totals over physical (Wi-Fi/Ethernet) interfaces.
    pub rx_bps: f64,
    pub tx_bps: f64,
}

impl NetInfo {
    pub fn primary(&self) -> Option<&Iface> {
        let gw = self.gateway.as_ref().map(|g| g.iface.as_str());
        self.ifaces
            .iter()
            .find(|i| Some(i.name.as_str()) == gw)
            .or_else(|| {
                self.ifaces
                    .iter()
                    .find(|i| i.up && matches!(i.kind, IfaceKind::Wifi | IfaceKind::Ethernet))
            })
    }
}

// ---------------------------------------------------------------------------------------
// Parsers
// ---------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct NetCounters {
    pub rx_bytes: u64,
    pub rx_errs: u64,
    pub rx_drop: u64,
    pub tx_bytes: u64,
    pub tx_errs: u64,
    pub tx_drop: u64,
}

/// Parse `/proc/net/dev`.
pub fn parse_net_dev(text: &str) -> Vec<(String, NetCounters)> {
    let mut out = Vec::new();
    for line in text.lines().skip(2) {
        let Some((name, rest)) = line.split_once(':') else {
            continue;
        };
        let f: Vec<u64> = rest
            .split_whitespace()
            .map(|t| t.parse().unwrap_or(0))
            .collect();
        if f.len() < 12 {
            continue;
        }
        out.push((
            name.trim().to_string(),
            NetCounters {
                rx_bytes: f[0],
                rx_errs: f[2],
                rx_drop: f[3],
                tx_bytes: f[8],
                tx_errs: f[10],
                tx_drop: f[11],
            },
        ));
    }
    out
}

/// Count TCP sockets by state in `/proc/net/tcp{,6}` text, without allocating per line.
pub fn count_tcp(buf: &[u8], s: &mut Sockets) {
    for line in buf.split(|b| *b == b'\n').skip(1) {
        let Some(st) = line.split(|b| *b == b' ').filter(|t| !t.is_empty()).nth(3) else {
            continue;
        };
        match st {
            b"01" => s.established += 1,
            b"0A" => s.listen += 1,
            b"06" => s.time_wait += 1,
            _ => s.other += 1,
        }
    }
}

/// Number of sockets (data lines) in `/proc/net/udp{,6}`.
pub fn count_udp(buf: &[u8]) -> u32 {
    buf.split(|b| *b == b'\n')
        .skip(1)
        .filter(|l| !l.iter().all(u8::is_ascii_whitespace))
        .count() as u32
}

/// Default route: `(interface, gateway)` with the lowest metric.
pub fn parse_default_route(text: &str) -> Option<Gateway> {
    let mut best: Option<(u32, Gateway)> = None;
    for line in text.lines().skip(1) {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 8 || f[1] != "00000000" || f[7] != "00000000" {
            continue;
        }
        let flags = u32::from_str_radix(f[3], 16).unwrap_or(0);
        if flags & 0x2 == 0 {
            continue;
        }
        let (Ok(gw), Ok(metric)) = (u32::from_str_radix(f[2], 16), f[6].parse::<u32>()) else {
            continue;
        };
        // The kernel prints the address as a host-order read of network-order bytes.
        let ip = Ipv4Addr::from(gw.to_be()).to_string();
        if best.as_ref().is_none_or(|(m, _)| metric < *m) {
            best = Some((
                metric,
                Gateway {
                    iface: f[0].to_string(),
                    ip,
                },
            ));
        }
    }
    best.map(|(_, g)| g)
}

/// `/proc/net/wireless`: interface -> signal level in dBm.
pub fn parse_wireless(text: &str) -> HashMap<String, i32> {
    let mut out = HashMap::new();
    for line in text.lines().skip(2) {
        let Some((name, rest)) = line.split_once(':') else {
            continue;
        };
        let f: Vec<&str> = rest.split_whitespace().collect();
        let Some(level) = f
            .get(2)
            .and_then(|l| l.trim_end_matches('.').parse::<i32>().ok())
        else {
            continue;
        };
        // Drivers reporting 0..255 instead of dBm use an offset of 256.
        let dbm = if level > 0 { level - 256 } else { level };
        out.insert(name.trim().to_string(), dbm);
    }
    out
}

/// Established and listening TCP sockets of `/proc/net/tcp{,6}` keyed by inode:
/// value is `Some(port)` for a listener, `None` for an established connection.
pub fn parse_tcp_inodes(text: &str, out: &mut HashMap<u64, Option<u16>>) {
    for line in text.lines().skip(1) {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 10 {
            continue;
        }
        let Ok(inode) = f[9].parse::<u64>() else {
            continue;
        };
        if inode == 0 {
            continue;
        }
        match f[3] {
            "01" => {
                out.insert(inode, None);
            }
            "0A" => {
                let port = f[1]
                    .rsplit(':')
                    .next()
                    .and_then(|p| u16::from_str_radix(p, 16).ok());
                if let Some(p) = port {
                    out.insert(inode, Some(p));
                }
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------------------
// System helpers
// ---------------------------------------------------------------------------------------

fn read_str(p: impl AsRef<Path>) -> Option<String> {
    fs::read_to_string(p).ok().map(|s| s.trim().to_string())
}

fn kind_of(name: &str) -> IfaceKind {
    let base = format!("/sys/class/net/{name}");
    if Path::new(&format!("{base}/wireless")).exists() {
        IfaceKind::Wifi
    } else if !Path::new(&format!("{base}/device")).exists() {
        let tunnel = ["tun", "tap", "wg", "tailscale", "ppp", "zt"];
        if tunnel.iter().any(|p| name.starts_with(p)) {
            IfaceKind::Tunnel
        } else {
            IfaceKind::Virtual
        }
    } else {
        IfaceKind::Ethernet
    }
}

#[derive(Clone)]
struct StaticInfo {
    kind: IfaceKind,
    speed_mbps: Option<u32>,
    mac: Option<String>,
}

fn static_info(name: &str) -> StaticInfo {
    let base = format!("/sys/class/net/{name}");
    StaticInfo {
        kind: kind_of(name),
        // -1 or a read error (unplugged, Wi-Fi) means unknown.
        speed_mbps: read_str(format!("{base}/speed"))
            .and_then(|s| s.parse::<i64>().ok())
            .filter(|s| *s > 0)
            .map(|s| s as u32),
        mac: read_str(format!("{base}/address")).filter(|m| m != "00:00:00:00:00:00"),
    }
}

type IpMap = HashMap<String, (Vec<String>, Vec<String>)>;

fn read_ips() -> IpMap {
    let mut map = IpMap::new();
    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs allocates a linked list that we only read and then free; every
    // sockaddr pointer is checked for null and family before being cast.
    unsafe {
        if libc::getifaddrs(&mut head) != 0 {
            return map;
        }
        let mut cur = head;
        while !cur.is_null() {
            let a = &*cur;
            cur = a.ifa_next;
            if a.ifa_addr.is_null() || a.ifa_name.is_null() {
                continue;
            }
            let name = std::ffi::CStr::from_ptr(a.ifa_name)
                .to_string_lossy()
                .into_owned();
            let family = i32::from((*a.ifa_addr).sa_family);
            if family == libc::AF_INET {
                let ip = Ipv4Addr::from(u32::from_be(
                    (*(a.ifa_addr as *const libc::sockaddr_in)).sin_addr.s_addr,
                ));
                let prefix = if a.ifa_netmask.is_null() {
                    32
                } else {
                    u32::from_be(
                        (*(a.ifa_netmask as *const libc::sockaddr_in))
                            .sin_addr
                            .s_addr,
                    )
                    .count_ones()
                };
                map.entry(name)
                    .or_default()
                    .0
                    .push(format!("{ip}/{prefix}"));
            } else if family == libc::AF_INET6 {
                let ip = Ipv6Addr::from(
                    (*(a.ifa_addr as *const libc::sockaddr_in6))
                        .sin6_addr
                        .s6_addr,
                );
                let prefix: u32 = if a.ifa_netmask.is_null() {
                    128
                } else {
                    (*(a.ifa_netmask as *const libc::sockaddr_in6))
                        .sin6_addr
                        .s6_addr
                        .iter()
                        .map(|b| b.count_ones())
                        .sum()
                };
                map.entry(name)
                    .or_default()
                    .1
                    .push(format!("{ip}/{prefix}"));
            }
        }
        libc::freeifaddrs(head);
    }
    map
}

// ---------------------------------------------------------------------------------------
// Per-app connection scanner
// ---------------------------------------------------------------------------------------

struct AppScanner {
    /// pid -> fd numbers that were sockets at the last full discovery.
    fds: HashMap<u32, Vec<u32>>,
    last_full: Option<Instant>,
}

impl AppScanner {
    fn discover(pid: u32) -> Vec<u32> {
        let Ok(rd) = fs::read_dir(format!("/proc/{pid}/fd")) else {
            return Vec::new();
        };
        let mut v = Vec::new();
        for e in rd.flatten() {
            let Ok(fd) = e.file_name().to_string_lossy().parse::<u32>() else {
                continue;
            };
            if let Ok(t) = fs::read_link(e.path())
                && t.to_string_lossy().starts_with("socket:[")
            {
                v.push(fd);
            }
        }
        v
    }

    /// Top applications by established connections. Only processes we may inspect (our own
    /// unless running as root) show up.
    fn scan(&mut self) -> Vec<ConnApp> {
        let mut inodes: HashMap<u64, Option<u16>> = HashMap::new();
        for f in ["/proc/net/tcp", "/proc/net/tcp6"] {
            if let Ok(t) = fs::read_to_string(f) {
                parse_tcp_inodes(&t, &mut inodes);
            }
        }
        if inodes.is_empty() {
            return Vec::new();
        }
        let full = self
            .last_full
            .is_none_or(|t| t.elapsed() >= APPS_FULL_EVERY);
        if full {
            self.last_full = Some(Instant::now());
        }
        let pids: Vec<u32> = fs::read_dir("/proc")
            .map(|rd| {
                rd.flatten()
                    .filter_map(|e| e.file_name().to_string_lossy().parse().ok())
                    .collect()
            })
            .unwrap_or_default();
        let alive: std::collections::HashSet<u32> = pids.iter().copied().collect();
        self.fds.retain(|p, _| alive.contains(p));
        let mut by_name: HashMap<String, ConnApp> = HashMap::new();
        for pid in pids {
            if full || !self.fds.contains_key(&pid) {
                self.fds.insert(pid, Self::discover(pid));
            }
            let mut est = 0u32;
            let mut ports: Vec<u16> = Vec::new();
            for fd in &self.fds[&pid] {
                let Ok(t) = fs::read_link(format!("/proc/{pid}/fd/{fd}")) else {
                    continue;
                };
                let t = t.to_string_lossy();
                let Some(inode) = t
                    .strip_prefix("socket:[")
                    .and_then(|r| r.strip_suffix(']'))
                    .and_then(|r| r.parse::<u64>().ok())
                else {
                    continue;
                };
                match inodes.get(&inode) {
                    Some(None) => est += 1,
                    Some(Some(p)) => ports.push(*p),
                    None => {}
                }
            }
            if est == 0 && ports.is_empty() {
                continue;
            }
            let name =
                read_str(format!("/proc/{pid}/comm")).unwrap_or_else(|| format!("pid {pid}"));
            let e = by_name.entry(name.clone()).or_insert_with(|| ConnApp {
                name,
                ..Default::default()
            });
            e.established += est;
            e.listening.extend(ports);
        }
        let mut apps: Vec<ConnApp> = by_name.into_values().collect();
        for a in &mut apps {
            a.listening.sort_unstable();
            a.listening.dedup();
        }
        apps.sort_by(|a, b| {
            b.established
                .cmp(&a.established)
                .then(b.listening.len().cmp(&a.listening.len()))
                .then(a.name.cmp(&b.name))
        });
        apps.truncate(5);
        apps
    }
}

// ---------------------------------------------------------------------------------------
// Sampler
// ---------------------------------------------------------------------------------------

pub struct NetSampler {
    prev: HashMap<String, NetCounters>,
    prev_at: Option<Instant>,
    info: HashMap<String, StaticInfo>,
    ips: IpMap,
    gateway: Option<Gateway>,
    slow_at: Option<Instant>,
    apps: Vec<ConnApp>,
    apps_at: Option<Instant>,
    sockets: Sockets,
    sockets_at: Option<Instant>,
    scanner: AppScanner,
    buf: Vec<u8>,
}

fn read_into(path: &str, buf: &mut Vec<u8>) -> bool {
    use std::io::Read;
    buf.clear();
    fs::File::open(path)
        .and_then(|mut f| f.read_to_end(buf))
        .is_ok()
}

impl NetSampler {
    pub fn new() -> Self {
        Self {
            prev: HashMap::new(),
            prev_at: None,
            info: HashMap::new(),
            ips: IpMap::new(),
            gateway: None,
            slow_at: None,
            apps: Vec::new(),
            apps_at: None,
            sockets: Sockets::default(),
            sockets_at: None,
            scanner: AppScanner {
                fds: HashMap::new(),
                last_full: None,
            },
            buf: Vec::with_capacity(64 * 1024),
        }
    }

    pub fn sample(&mut self) -> NetInfo {
        let now = Instant::now();
        if self.slow_at.is_none_or(|t| t.elapsed() >= SLOW_EVERY) {
            self.ips = read_ips();
            self.gateway = fs::read_to_string("/proc/net/route")
                .ok()
                .and_then(|t| parse_default_route(&t));
            self.info.clear();
            self.slow_at = Some(now);
        }
        let counters = parse_net_dev(&fs::read_to_string("/proc/net/dev").unwrap_or_default());
        let dt = self
            .prev_at
            .map(|t| now.duration_since(t).as_secs_f64())
            .unwrap_or(0.0);
        let wireless = fs::read_to_string("/proc/net/wireless")
            .map(|t| parse_wireless(&t))
            .unwrap_or_default();
        let mut ifaces = Vec::new();
        for (name, c) in &counters {
            if name == "lo" {
                continue;
            }
            let info = self
                .info
                .entry(name.clone())
                .or_insert_with(|| static_info(name))
                .clone();
            let operstate =
                read_str(format!("/sys/class/net/{name}/operstate")).unwrap_or_default();
            let up = matches!(operstate.as_str(), "up" | "unknown");
            let total = c.rx_bytes + c.tx_bytes;
            if !up && total == 0 && info.kind != IfaceKind::Ethernet {
                continue;
            }
            let (rx_bps, tx_bps) = match self.prev.get(name) {
                Some(p) if dt > 0.0 => (
                    c.rx_bytes.saturating_sub(p.rx_bytes) as f64 / dt,
                    c.tx_bytes.saturating_sub(p.tx_bytes) as f64 / dt,
                ),
                _ => (0.0, 0.0),
            };
            let (ipv4, ipv6) = self.ips.get(name).cloned().unwrap_or_default();
            ifaces.push(Iface {
                name: name.clone(),
                kind: info.kind,
                up,
                operstate,
                speed_mbps: info.speed_mbps,
                rx_bps,
                tx_bps,
                rx_total: c.rx_bytes,
                tx_total: c.tx_bytes,
                errors: c.rx_errs + c.tx_errs,
                drops: c.rx_drop + c.tx_drop,
                signal_dbm: wireless.get(name).copied(),
                mac: info.mac,
                ipv4,
                ipv6,
            });
        }
        self.prev = counters.into_iter().collect();
        self.prev_at = Some(now);

        let gw = self.gateway.as_ref().map(|g| g.iface.clone());
        ifaces.sort_by(|a, b| {
            (Some(&b.name) == gw.as_ref())
                .cmp(&(Some(&a.name) == gw.as_ref()))
                .then(b.up.cmp(&a.up))
                .then(a.name.cmp(&b.name))
        });
        let (mut rx, mut tx) = (0.0, 0.0);
        for i in ifaces
            .iter()
            .filter(|i| matches!(i.kind, IfaceKind::Wifi | IfaceKind::Ethernet))
        {
            rx += i.rx_bps;
            tx += i.tx_bps;
        }

        // The kernel walks its socket tables for every read of /proc/net/tcp*, which is the
        // costliest thing here: refresh the counts every few seconds only.
        if self.sockets_at.is_none_or(|t| t.elapsed() >= SOCKETS_EVERY) {
            let mut sockets = Sockets::default();
            for f in ["/proc/net/tcp", "/proc/net/tcp6"] {
                if read_into(f, &mut self.buf) {
                    count_tcp(&self.buf, &mut sockets);
                }
            }
            for f in ["/proc/net/udp", "/proc/net/udp6"] {
                if read_into(f, &mut self.buf) {
                    sockets.udp += count_udp(&self.buf);
                }
            }
            self.sockets = sockets;
            self.sockets_at = Some(Instant::now());
        }
        if self.apps_at.is_none_or(|t| t.elapsed() >= APPS_EVERY) {
            self.apps = self.scanner.scan();
            self.apps_at = Some(Instant::now());
        }
        NetInfo {
            ifaces,
            gateway: self.gateway.clone(),
            sockets: self.sockets,
            apps: self.apps.clone(),
            rx_bps: rx,
            tx_bps: tx,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NET_DEV: &str = "\
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
    lo: 1000 10 0 0 0 0 0 0 1000 10 0 0 0 0 0 0
wlan0:4567890 3000 2 5 0 0 0 0 1234567 2000 1 3 0 0 0 0
";

    #[test]
    fn net_dev_parsing() {
        let v = parse_net_dev(NET_DEV);
        assert_eq!(v.len(), 2);
        assert_eq!(v[1].0, "wlan0");
        let c = v[1].1;
        assert_eq!((c.rx_bytes, c.rx_errs, c.rx_drop), (4_567_890, 2, 5));
        assert_eq!((c.tx_bytes, c.tx_errs, c.tx_drop), (1_234_567, 1, 3));
    }

    #[test]
    fn tcp_state_counting() {
        let tcp = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
   0: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 11111 1 0 100 0\n\
   1: 8E01A8C0:C000 0100007F:01BB 01 00000000:00000000 00:00000000 00000000  1000        0 22222 1 0 100 0\n\
   2: 8E01A8C0:C001 0100007F:01BB 01 00000000:00000000 00:00000000 00000000  1000        0 33333 1 0 100 0\n\
   3: 8E01A8C0:C002 0100007F:01BB 06 00000000:00000000 00:00000000 00000000     0        0 0 1 0 100 0\n\
   4: 8E01A8C0:C003 0100007F:01BB 08 00000000:00000000 00:00000000 00000000  1000        0 44444 1 0 100 0\n";
        let mut s = Sockets::default();
        count_tcp(tcp.as_bytes(), &mut s);
        assert_eq!(
            (s.established, s.listen, s.time_wait, s.other),
            (2, 1, 1, 1)
        );
        assert_eq!(s.tcp_total(), 5);
        assert_eq!(count_udp(b"header\n a b c\n d e f\n\n"), 2);
        let mut m = HashMap::new();
        parse_tcp_inodes(tcp, &mut m);
        assert_eq!(m.get(&11111), Some(&Some(8080)));
        assert_eq!(m.get(&22222), Some(&None));
        assert!(!m.contains_key(&44444));
    }

    #[test]
    fn route_and_wireless_parsing() {
        let route = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\tMTU\tWindow\tIRTT\n\
eno1\t00000000\t0101A8C0\t0003\t0\t0\t700\t00000000\t0\t0\t0\n\
wlan0\t00000000\t0102A8C0\t0003\t0\t0\t600\t00000000\t0\t0\t0\n\
wlan0\t0001A8C0\t00000000\t0001\t0\t0\t600\t00FFFFFF\t0\t0\t0\n";
        let g = parse_default_route(route).unwrap();
        assert_eq!((g.iface.as_str(), g.ip.as_str()), ("wlan0", "192.168.2.1"));
        assert!(parse_default_route("Iface\n").is_none());
        let w = "Inter-| sta-|   Quality        |\n face | tus | link level noise |\n wlan0: 0000   49.  -61.  -256        0      0      0     23    227        0\n";
        assert_eq!(parse_wireless(w).get("wlan0"), Some(&-61));
    }
}
