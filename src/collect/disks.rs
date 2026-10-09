//! Disk sampler: per-device I/O rates from /proc/diskstats, mounted filesystems via statvfs,
//! and SMART health from a background smartctl worker (never sudo).

use std::collections::HashMap;
use std::ffi::CString;
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const SLOW_EVERY: Duration = Duration::from_secs(10);
const SMART_EVERY: Duration = Duration::from_secs(600);
const REAL_FS: [&str; 8] = [
    "btrfs", "ext4", "xfs", "f2fs", "vfat", "exfat", "ntfs3", "zfs",
];

// ---------------------------------------------------------------------------------------
// Data
// ---------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Smart {
    pub passed: Option<bool>,
    pub temp_c: Option<f64>,
    pub power_on_hours: Option<u64>,
    pub pct_used: Option<f64>,
    pub avail_spare: Option<f64>,
    pub media_errors: Option<u64>,
    pub unsafe_shutdowns: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub enum SmartState {
    /// Worker has not reported yet.
    #[default]
    Pending,
    Ready(Smart),
    /// smartctl exists but the device could not be opened without root.
    NeedsRoot,
    /// smartctl missing or the device does not report SMART.
    Unavailable,
}

#[derive(Debug, Clone, Default)]
pub struct DiskIo {
    pub name: String,
    pub model: Option<String>,
    pub size: u64,
    pub rotational: bool,
    pub read_bps: f64,
    pub write_bps: f64,
    pub read_iops: f64,
    pub write_iops: f64,
    pub read_lat_ms: Option<f64>,
    pub write_lat_ms: Option<f64>,
    pub busy_pct: f64,
    pub inflight: u64,
    pub smart: SmartState,
}

impl DiskIo {
    /// `NVMe SSD`, `SSD` or `HDD`.
    pub fn kind(&self) -> &'static str {
        if self.name.starts_with("nvme") {
            "NVMe SSD"
        } else if self.rotational {
            "HDD"
        } else {
            "SSD"
        }
    }
}

/// One filesystem (btrfs subvolumes of a device are merged into `points`).
#[derive(Debug, Clone, Default)]
pub struct Mount {
    pub points: Vec<String>,
    pub source: String,
    pub fstype: String,
    /// Physical disk name, or empty when unknown.
    pub disk: String,
    pub total: u64,
    pub used: u64,
    pub avail: u64,
}

impl Mount {
    /// Percent full, like `df`: used / (used + available).
    pub fn used_pct(&self) -> f64 {
        let d = self.used + self.avail;
        if d == 0 {
            0.0
        } else {
            self.used as f64 / d as f64 * 100.0
        }
    }
    pub fn label(&self) -> String {
        self.points.join(", ")
    }
}

#[derive(Debug, Clone, Default)]
pub struct DisksInfo {
    pub disks: Vec<DiskIo>,
    pub mounts: Vec<Mount>,
}

impl DisksInfo {
    pub fn read_bps(&self) -> f64 {
        self.disks.iter().map(|d| d.read_bps).sum()
    }
    pub fn write_bps(&self) -> f64 {
        self.disks.iter().map(|d| d.write_bps).sum()
    }
}

// ---------------------------------------------------------------------------------------
// Parsers
// ---------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DiskStat {
    pub reads: u64,
    pub sectors_read: u64,
    pub ms_reading: u64,
    pub writes: u64,
    pub sectors_written: u64,
    pub ms_writing: u64,
    pub inflight: u64,
    pub io_ticks: u64,
}

/// Names that can be physical block devices (partitions are excluded separately).
pub fn is_candidate_name(n: &str) -> bool {
    ["nvme", "sd", "vd", "mmcblk"]
        .iter()
        .any(|p| n.starts_with(p))
}

/// Parse `/proc/diskstats` lines of candidate device names.
pub fn parse_diskstats(text: &str) -> Vec<(String, DiskStat)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 14 || !is_candidate_name(f[2]) {
            continue;
        }
        let n = |i: usize| f[i].parse::<u64>().unwrap_or(0);
        out.push((
            f[2].to_string(),
            DiskStat {
                reads: n(3),
                sectors_read: n(5),
                ms_reading: n(6),
                writes: n(7),
                sectors_written: n(9),
                ms_writing: n(10),
                inflight: n(11),
                io_ticks: n(12),
            },
        ));
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct IoRates {
    pub read_bps: f64,
    pub write_bps: f64,
    pub read_iops: f64,
    pub write_iops: f64,
    pub read_lat_ms: Option<f64>,
    pub write_lat_ms: Option<f64>,
    pub busy_pct: f64,
}

/// Rates between two samples `dt` seconds apart. A counter that went backwards means the
/// device was reset or re-created: report zero for that sample.
pub fn io_rates(prev: &DiskStat, cur: &DiskStat, dt: f64) -> IoRates {
    if dt <= 0.0 {
        return IoRates::default();
    }
    let d = |c: u64, p: u64| c.checked_sub(p);
    let (Some(dr), Some(dw), Some(sr), Some(sw)) = (
        d(cur.reads, prev.reads),
        d(cur.writes, prev.writes),
        d(cur.sectors_read, prev.sectors_read),
        d(cur.sectors_written, prev.sectors_written),
    ) else {
        return IoRates::default();
    };
    let lat = |c: u64, p: u64, ops: u64| (ops > 0).then(|| c.saturating_sub(p) as f64 / ops as f64);
    IoRates {
        read_bps: sr as f64 * 512.0 / dt,
        write_bps: sw as f64 * 512.0 / dt,
        read_iops: dr as f64 / dt,
        write_iops: dw as f64 / dt,
        read_lat_ms: lat(cur.ms_reading, prev.ms_reading, dr),
        write_lat_ms: lat(cur.ms_writing, prev.ms_writing, dw),
        busy_pct: (cur.io_ticks.saturating_sub(prev.io_ticks) as f64 / (dt * 1000.0) * 100.0)
            .clamp(0.0, 100.0),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountEntry {
    pub source: String,
    pub mount_point: String,
    pub fstype: String,
}

/// Decode `\040`-style octal escapes used by the kernel in mountinfo paths.
fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && i + 4 <= b.len()
            && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c))
        {
            let v = (b[i + 1] - b'0') as u32 * 64
                + (b[i + 2] - b'0') as u32 * 8
                + (b[i + 3] - b'0') as u32;
            out.push(v as u8);
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Parse `/proc/self/mountinfo`, keeping only real filesystems.
pub fn parse_mountinfo(text: &str) -> Vec<MountEntry> {
    let mut out = Vec::new();
    for line in text.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        let Some(dash) = f.iter().position(|t| *t == "-") else {
            continue;
        };
        if dash < 5 || f.len() < dash + 3 || !REAL_FS.contains(&f[dash + 1]) {
            continue;
        }
        out.push(MountEntry {
            source: unescape(f[dash + 2]),
            mount_point: unescape(f[4]),
            fstype: f[dash + 1].to_string(),
        });
    }
    out
}

/// One row per source device; mount points sorted shortest first.
pub fn group_mounts(entries: &[MountEntry]) -> Vec<(String, String, Vec<String>)> {
    let mut out: Vec<(String, String, Vec<String>)> = Vec::new();
    for e in entries {
        match out.iter_mut().find(|(s, _, _)| *s == e.source) {
            Some((_, _, pts)) => {
                if !pts.contains(&e.mount_point) {
                    pts.push(e.mount_point.clone());
                }
            }
            None => out.push((
                e.source.clone(),
                e.fstype.clone(),
                vec![e.mount_point.clone()],
            )),
        }
    }
    for (_, _, pts) in &mut out {
        pts.sort_by(|a, b| a.len().cmp(&b.len()).then(a.cmp(b)));
    }
    out.sort_by(|a, b| a.2[0].len().cmp(&b.2[0].len()).then(a.2[0].cmp(&b.2[0])));
    out
}

// ---- Minimal targeted JSON extraction (smartctl -j) ----

/// Text following `"key":` (after whitespace), searching from the start of `s`.
pub(crate) fn raw_after<'a>(s: &'a str, key: &str) -> Option<&'a str> {
    let pat = format!("\"{key}\"");
    let mut from = 0;
    while let Some(i) = s[from..].find(&pat) {
        let rest = s[from + i + pat.len()..].trim_start();
        if let Some(v) = rest.strip_prefix(':') {
            return Some(v.trim_start());
        }
        from += i + pat.len();
    }
    None
}

pub(crate) fn leading_num(s: &str) -> Option<f64> {
    let end = s
        .find(|c: char| !(c.is_ascii_digit() || matches!(c, '-' | '+' | '.' | 'e' | 'E')))
        .unwrap_or(s.len());
    s[..end].parse().ok()
}

pub(crate) fn num_after(s: &str, key: &str) -> Option<f64> {
    leading_num(raw_after(s, key)?)
}

/// Extract the health fields from `smartctl -j -H -A` output.
pub fn parse_smart(json: &str) -> SmartState {
    if json.contains("Permission denied") {
        return SmartState::NeedsRoot;
    }
    let passed = raw_after(json, "smart_status")
        .and_then(|r| raw_after(r, "passed"))
        .map(|v| v.starts_with("true"));
    let nvme = raw_after(json, "nvme_smart_health_information_log");
    let temp = match raw_after(json, "temperature") {
        Some(r) if r.starts_with('{') => num_after(r, "current"),
        Some(r) => leading_num(r),
        None => nvme.and_then(|n| num_after(n, "temperature")),
    };
    let hours = raw_after(json, "power_on_time")
        .and_then(|r| num_after(r, "hours"))
        .or_else(|| nvme.and_then(|n| num_after(n, "power_on_hours")));
    let s = Smart {
        passed,
        temp_c: temp,
        power_on_hours: hours.map(|h| h as u64),
        pct_used: nvme.and_then(|n| num_after(n, "percentage_used")),
        avail_spare: nvme.and_then(|n| num_after(n, "available_spare")),
        media_errors: nvme
            .and_then(|n| num_after(n, "media_errors"))
            .map(|v| v as u64),
        unsafe_shutdowns: nvme
            .and_then(|n| num_after(n, "unsafe_shutdowns"))
            .map(|v| v as u64),
    };
    if s.passed.is_none() && s.temp_c.is_none() && s.power_on_hours.is_none() {
        SmartState::Unavailable
    } else {
        SmartState::Ready(s)
    }
}

// ---------------------------------------------------------------------------------------
// System helpers
// ---------------------------------------------------------------------------------------

fn read_str(p: impl AsRef<Path>) -> Option<String> {
    fs::read_to_string(p).ok().map(|s| s.trim().to_string())
}

/// Whole physical block devices (nvme*, sd*, vd*, mmcblk*), never partitions.
pub fn physical_disks() -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir("/sys/block")
        .map(|rd| {
            rd.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| {
                    is_candidate_name(n)
                        && !Path::new(&format!("/sys/class/block/{n}/partition")).exists()
                })
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

/// Physical disk behind a block device name (partition -> parent, dm -> first slave).
fn parent_disk(name: &str, depth: u8) -> String {
    let base = format!("/sys/class/block/{name}");
    if Path::new(&format!("{base}/partition")).exists()
        && let Ok(real) = fs::canonicalize(&base)
        && let Some(parent) = real.parent().and_then(|p| p.file_name())
    {
        return parent.to_string_lossy().into_owned();
    }
    if depth < 3
        && let Ok(rd) = fs::read_dir(format!("{base}/slaves"))
        && let Some(slave) = rd.flatten().map(|e| e.file_name()).min()
    {
        return parent_disk(&slave.to_string_lossy(), depth + 1);
    }
    name.to_string()
}

fn statvfs(path: &str) -> Option<(u64, u64, u64)> {
    let c = CString::new(path).ok()?;
    // SAFETY: statvfs writes into our zeroed struct; the path is a valid C string.
    let s = unsafe {
        let mut s: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c.as_ptr(), &mut s) != 0 {
            return None;
        }
        s
    };
    let fr = s.f_frsize;
    Some((
        s.f_blocks * fr,
        s.f_blocks.saturating_sub(s.f_bfree) * fr,
        s.f_bavail * fr,
    ))
}

fn read_mounts() -> Vec<Mount> {
    let Ok(text) = fs::read_to_string("/proc/self/mountinfo") else {
        return Vec::new();
    };
    group_mounts(&parse_mountinfo(&text))
        .into_iter()
        .filter_map(|(source, fstype, points)| {
            let (total, used, avail) = statvfs(&points[0])?;
            let disk = if source.starts_with("/dev/") {
                fs::canonicalize(&source)
                    .ok()
                    .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                    .map(|n| parent_disk(&n, 0))
                    .unwrap_or_default()
            } else {
                String::new()
            };
            Some(Mount {
                points,
                source,
                fstype,
                disk,
                total,
                used,
                avail,
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------------------
// SMART worker
// ---------------------------------------------------------------------------------------

type SmartMap = Arc<Mutex<HashMap<String, SmartState>>>;

fn smart_worker(map: SmartMap) {
    loop {
        for disk in physical_disks() {
            let out = std::process::Command::new("smartctl")
                .args(["-j", "-H", "-A", "-n", "standby", &format!("/dev/{disk}")])
                .stdin(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .output();
            let state = match out {
                Err(_) => {
                    // smartctl is not installed: mark everything unavailable and stop.
                    if let Ok(mut m) = map.lock() {
                        for d in physical_disks() {
                            m.insert(d, SmartState::Unavailable);
                        }
                    }
                    return;
                }
                Ok(o) => {
                    let text = String::from_utf8_lossy(&o.stdout);
                    // A drive in standby is left alone; keep whatever we knew before.
                    if text.contains("STANDBY") {
                        continue;
                    }
                    parse_smart(&text)
                }
            };
            if let Ok(mut m) = map.lock() {
                m.insert(disk, state);
            }
        }
        std::thread::sleep(SMART_EVERY);
    }
}

// ---------------------------------------------------------------------------------------
// Sampler
// ---------------------------------------------------------------------------------------

struct Meta {
    name: String,
    model: Option<String>,
    size: u64,
    rotational: bool,
}

pub struct DiskSampler {
    prev: HashMap<String, DiskStat>,
    prev_at: Option<Instant>,
    meta: Vec<Meta>,
    mounts: Vec<Mount>,
    slow_at: Option<Instant>,
    smart: SmartMap,
}

impl DiskSampler {
    pub fn new() -> Self {
        let smart: SmartMap = Arc::default();
        let worker = Arc::clone(&smart);
        let _ = std::thread::Builder::new()
            .name("mstop-smart".into())
            .spawn(move || smart_worker(worker));
        Self {
            prev: HashMap::new(),
            prev_at: None,
            meta: Vec::new(),
            mounts: Vec::new(),
            slow_at: None,
            smart,
        }
    }

    fn refresh_slow(&mut self) {
        self.meta = physical_disks()
            .into_iter()
            .map(|name| {
                let sys = format!("/sys/block/{name}");
                Meta {
                    model: read_str(format!("{sys}/device/model")).filter(|m| !m.is_empty()),
                    size: read_str(format!("{sys}/size"))
                        .and_then(|s| s.parse::<u64>().ok())
                        .unwrap_or(0)
                        * 512,
                    rotational: read_str(format!("{sys}/queue/rotational")).as_deref() == Some("1"),
                    name,
                }
            })
            .collect();
        self.mounts = read_mounts();
        self.slow_at = Some(Instant::now());
    }

    pub fn sample(&mut self) -> DisksInfo {
        let now = Instant::now();
        if self.slow_at.is_none_or(|t| t.elapsed() >= SLOW_EVERY) {
            self.refresh_slow();
        }
        let stats: HashMap<String, DiskStat> =
            parse_diskstats(&fs::read_to_string("/proc/diskstats").unwrap_or_default())
                .into_iter()
                .collect();
        let dt = self
            .prev_at
            .map(|t| now.duration_since(t).as_secs_f64())
            .unwrap_or(0.0);
        let smart = self.smart.lock().map(|m| m.clone()).unwrap_or_default();
        let mut disks = Vec::new();
        for m in &self.meta {
            let Some(cur) = stats.get(&m.name) else {
                continue;
            };
            let r = self
                .prev
                .get(&m.name)
                .map(|p| io_rates(p, cur, dt))
                .unwrap_or_default();
            disks.push(DiskIo {
                name: m.name.clone(),
                model: m.model.clone(),
                size: m.size,
                rotational: m.rotational,
                read_bps: r.read_bps,
                write_bps: r.write_bps,
                read_iops: r.read_iops,
                write_iops: r.write_iops,
                read_lat_ms: r.read_lat_ms,
                write_lat_ms: r.write_lat_ms,
                busy_pct: r.busy_pct,
                inflight: cur.inflight,
                smart: smart.get(&m.name).cloned().unwrap_or_default(),
            });
        }
        self.prev = stats;
        self.prev_at = Some(now);
        DisksInfo {
            disks,
            mounts: self.mounts.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DISKSTATS: &str = "\
 259       0 nvme0n1 1000 5 20480 300 500 7 8192 550 2 400 900 0 0 0 0 0 0
 259       1 nvme0n1p1 10 0 80 1 2 0 16 1 0 2 2 0 0 0 0 0 0
   7       0 loop0 5 0 10 0 0 0 0 0 0 0 0 0 0 0 0 0 0
 252       0 zram0 1 0 8 0 0 0 0 0 0 0 0 0 0 0 0 0 0
   8       0 sda 7 0 56 7 3 0 24 6 0 5 13 0 0 0 0 0 0
";

    #[test]
    fn diskstats_parsing() {
        let v = parse_diskstats(DISKSTATS);
        let names: Vec<&str> = v.iter().map(|(n, _)| n.as_str()).collect();
        // Partitions are filtered later via sysfs; loop and zram are dropped here.
        assert_eq!(names, ["nvme0n1", "nvme0n1p1", "sda"]);
        let s = v[0].1;
        assert_eq!((s.reads, s.sectors_read, s.ms_reading), (1000, 20480, 300));
        assert_eq!(
            (s.writes, s.sectors_written, s.ms_writing),
            (500, 8192, 550)
        );
        assert_eq!((s.inflight, s.io_ticks), (2, 400));
    }

    #[test]
    fn rates_and_reset() {
        let a = DiskStat {
            reads: 100,
            sectors_read: 1000,
            ms_reading: 50,
            writes: 10,
            sectors_written: 200,
            ms_writing: 20,
            io_ticks: 100,
            ..Default::default()
        };
        let b = DiskStat {
            reads: 300,
            sectors_read: 3048,
            ms_reading: 110,
            writes: 10,
            sectors_written: 200,
            ms_writing: 20,
            io_ticks: 600,
            ..Default::default()
        };
        let r = io_rates(&a, &b, 2.0);
        assert_eq!(r.read_bps, 2048.0 * 512.0 / 2.0);
        assert_eq!(r.read_iops, 100.0);
        assert!((r.read_lat_ms.unwrap() - 0.3).abs() < 1e-9);
        assert_eq!(r.write_lat_ms, None);
        assert_eq!(r.busy_pct, 25.0);
        // Counter going backwards: treated as a reset.
        assert_eq!(io_rates(&b, &a, 2.0), IoRates::default());
        assert_eq!(io_rates(&a, &b, 0.0), IoRates::default());
    }

    const MOUNTINFO: &str = "\
40 1 0:35 /@ / rw,noatime shared:1 - btrfs /dev/nvme0n1p3 rw,subvol=/@
60 40 0:35 /@home /home rw,noatime shared:183 - btrfs /dev/nvme0n1p3 rw,subvol=/@home
38 40 0:7 / /dev rw,nosuid shared:2 - devtmpfs devtmpfs rw
224 40 259:1 / /boot rw,relatime shared:188 - vfat /dev/nvme0n1p1 rw
300 40 8:17 / /mnt/My\\040Disk\\011x rw - ext4 /dev/sdb1 rw
301 40 0:50 / /run/user/1000 rw - tmpfs tmpfs rw
";

    #[test]
    fn mountinfo_parsing_and_grouping() {
        let v = parse_mountinfo(MOUNTINFO);
        assert_eq!(v.len(), 4);
        assert_eq!(v[3].mount_point, "/mnt/My Disk\tx");
        assert_eq!(v[3].fstype, "ext4");
        let g = group_mounts(&v);
        assert_eq!(g.len(), 3);
        assert_eq!(g[0].0, "/dev/nvme0n1p3");
        assert_eq!(g[0].2, ["/", "/home"]);
        assert_eq!(g[1].2, ["/boot"]);
    }

    const SMART_NVME: &str = r#"{
  "smart_status": { "passed": true },
  "nvme_smart_health_information_log": {
    "critical_warning": 0, "temperature": 41, "available_spare": 100,
    "percentage_used": 3, "power_on_hours": 1234, "unsafe_shutdowns": 17,
    "media_errors": 0
  },
  "temperature": { "current": 41 },
  "power_on_time": { "hours": 1234 }
}"#;

    #[test]
    fn smart_extraction() {
        let SmartState::Ready(s) = parse_smart(SMART_NVME) else {
            panic!("expected data");
        };
        assert_eq!(s.passed, Some(true));
        assert_eq!(s.temp_c, Some(41.0));
        assert_eq!(s.power_on_hours, Some(1234));
        assert_eq!(s.pct_used, Some(3.0));
        assert_eq!(s.avail_spare, Some(100.0));
        assert_eq!(s.media_errors, Some(0));
        assert_eq!(s.unsafe_shutdowns, Some(17));
        let failing = r#"{"smart_status":{"passed":false},"temperature":{"current":50}}"#;
        let SmartState::Ready(f) = parse_smart(failing) else {
            panic!()
        };
        assert_eq!(f.passed, Some(false));
        assert_eq!(f.pct_used, None);
        let denied = r#"{"smartctl":{"messages":[{"string":"Smartctl open device: /dev/nvme0n1 failed: Permission denied"}]}}"#;
        assert_eq!(parse_smart(denied), SmartState::NeedsRoot);
        assert_eq!(parse_smart("{}"), SmartState::Unavailable);
    }
}
