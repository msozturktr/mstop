//! GPU sampler: sysfs for AMD/Intel, NVML (or nvidia-smi) for NVIDIA, DRM fdinfo per process.
//!
//! A runtime-suspended GPU is never touched: when `power/runtime_status` says `suspended`
//! the GPU is reported as sleeping and nothing else on it is read or queried.

use nvml_wrapper::Nvml;
use nvml_wrapper::enum_wrappers::device::{Clock, TemperatureSensor};
use nvml_wrapper::enums::device::UsedGpuMemory;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Vendor {
    Nvidia,
    Amd,
    Intel,
    #[default]
    Other,
}

impl Vendor {
    fn from_id(id: u32) -> Vendor {
        match id {
            0x10de => Vendor::Nvidia,
            0x1002 => Vendor::Amd,
            0x8086 => Vendor::Intel,
            _ => Vendor::Other,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Vendor::Nvidia => "NVIDIA",
            Vendor::Amd => "AMD",
            Vendor::Intel => "Intel",
            Vendor::Other => "GPU",
        }
    }
}

/// One application using a GPU.
#[derive(Debug, Clone, Default)]
pub struct GpuProc {
    pub pid: u32,
    pub name: String,
    pub gpu_pct: Option<f64>,
    pub vram: Option<u64>,
}

#[derive(Debug, Clone, Default)]
pub struct Gpu {
    pub index: usize,
    pub name: String,
    pub vendor: Vendor,
    pub pci: String,
    pub discrete: bool,
    /// Runtime-suspended: nothing else below is populated.
    pub sleeping: bool,
    pub util: Option<f64>,
    pub vram_used: Option<u64>,
    pub vram_total: Option<u64>,
    pub gtt_used: Option<u64>,
    pub gtt_total: Option<u64>,
    pub temp_c: Option<f64>,
    pub power_w: Option<f64>,
    pub power_cap_w: Option<f64>,
    pub fan_pct: Option<f64>,
    pub fan_rpm: Option<u32>,
    pub clock_mhz: Option<u32>,
    pub mem_clock_mhz: Option<u32>,
    pub pstate: Option<String>,
    /// Sorted by GPU usage, then VRAM (largest first).
    pub procs: Vec<GpuProc>,
}

// ---------------------------------------------------------------------------------------
// Parsing helpers
// ---------------------------------------------------------------------------------------

/// What we need from one `/proc/PID/fdinfo/N` of a DRM file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FdInfo {
    pub pdev: String,
    pub client: u64,
    pub engine_ns: u64,
    pub vram_bytes: u64,
}

fn first_num(v: &str) -> Option<u64> {
    v.split_whitespace().next()?.parse().ok()
}

/// Parse DRM fdinfo text. Returns None when it is not a DRM fdinfo (no `drm-pdev`).
pub fn parse_fdinfo(text: &str) -> Option<FdInfo> {
    let mut pdev = None;
    let mut client = 0u64;
    let mut engine_ns = 0u64;
    let mut vram = None;
    let mut resident = None;
    for line in text.lines() {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let v = v.trim();
        match k.trim() {
            "drm-pdev" => pdev = Some(v.to_string()),
            "drm-client-id" => client = v.parse().unwrap_or(0),
            "drm-memory-vram" => vram = first_num(v).map(|n| n * 1024),
            "drm-resident-vram" => resident = first_num(v).map(|n| n * 1024),
            k if k.starts_with("drm-engine-") && !k.starts_with("drm-engine-capacity-") => {
                engine_ns += first_num(v).unwrap_or(0);
            }
            _ => {}
        }
    }
    Some(FdInfo {
        pdev: pdev?,
        client,
        engine_ns,
        vram_bytes: vram.or(resident).unwrap_or(0),
    })
}

/// Current clock (MHz) from an `pp_dpm_*` table: the line marked with `*`.
pub fn parse_dpm_current(text: &str) -> Option<u32> {
    let line = text.lines().find(|l| l.trim_end().ends_with('*'))?;
    let after = line.split_once(':')?.1.trim();
    let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// Values reported by NVML or `nvidia-smi`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NvData {
    pub util: Option<f64>,
    pub mem_used: Option<u64>,
    pub mem_total: Option<u64>,
    pub temp: Option<f64>,
    pub power: Option<f64>,
    pub power_cap: Option<f64>,
    pub fan: Option<f64>,
    pub clk_gr: Option<u32>,
    pub clk_mem: Option<u32>,
    pub pstate: Option<String>,
    pub name: Option<String>,
    pub procs: Vec<(u32, Option<u64>)>,
}

fn smi_num(s: &str) -> Option<f64> {
    let s = s.trim();
    if s.is_empty() || s.contains("N/A") || s.contains("Not Supported") {
        None
    } else {
        s.parse().ok()
    }
}

/// Parse the CSV line of
/// `utilization.gpu,memory.used,memory.total,temperature.gpu,power.draw,power.limit,fan.speed,clocks.gr,clocks.mem,pstate`.
pub fn parse_smi_csv(line: &str) -> Option<NvData> {
    let f: Vec<&str> = line.trim().split(',').map(str::trim).collect();
    if f.len() < 10 {
        return None;
    }
    const MIB: f64 = 1024.0 * 1024.0;
    Some(NvData {
        util: smi_num(f[0]),
        mem_used: smi_num(f[1]).map(|m| (m * MIB) as u64),
        mem_total: smi_num(f[2]).map(|m| (m * MIB) as u64),
        temp: smi_num(f[3]),
        power: smi_num(f[4]),
        power_cap: smi_num(f[5]),
        fan: smi_num(f[6]),
        clk_gr: smi_num(f[7]).map(|c| c as u32),
        clk_mem: smi_num(f[8]).map(|c| c as u32),
        pstate: (!f[9].contains("N/A") && !f[9].is_empty()).then(|| f[9].to_string()),
        name: None,
        procs: Vec::new(),
    })
}

/// Look up a device name in pci.ids: the bracketed marketing name when present.
pub fn pci_ids_lookup(text: &str, vendor: u32, device: u32) -> Option<String> {
    let vkey = format!("{vendor:04x}");
    let dkey = format!("{device:04x}");
    let mut in_vendor = false;
    for line in text.lines() {
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        if !line.starts_with('\t') {
            if in_vendor {
                return None;
            }
            in_vendor = line.starts_with(&vkey);
            continue;
        }
        if in_vendor && !line.starts_with("\t\t") {
            let t = line.trim_start();
            if let Some(rest) = t.strip_prefix(&dkey) {
                let name = rest.trim();
                if let (Some(a), Some(b)) = (name.rfind('['), name.rfind(']'))
                    && a < b
                {
                    return Some(name[a + 1..b].to_string());
                }
                return Some(name.to_string());
            }
        }
    }
    None
}

fn read_str(p: impl AsRef<Path>) -> Option<String> {
    fs::read_to_string(p).ok().map(|s| s.trim().to_string())
}

fn read_u64(p: impl AsRef<Path>) -> Option<u64> {
    read_str(p)?.parse().ok()
}

fn read_hex(p: impl AsRef<Path>) -> Option<u32> {
    let s = read_str(p)?;
    u32::from_str_radix(s.trim_start_matches("0x"), 16).ok()
}

fn is_sleeping(device: &Path) -> bool {
    matches!(
        read_str(device.join("power/runtime_status")).as_deref(),
        Some("suspended") | Some("suspending")
    )
}

// ---------------------------------------------------------------------------------------
// NVIDIA backends
// ---------------------------------------------------------------------------------------

#[derive(Default)]
struct NvBackend {
    nvml: Option<Nvml>,
    nvml_failed: bool,
    smi_cache: Option<(Instant, Option<NvData>)>,
    idle: IdleState,
}

/// Samples of 0% utilization after which an otherwise idle NVIDIA GPU is polled slowly.
const IDLE_SAMPLES: u32 = 5;
/// Poll interval while backing off, so NVML does not keep resetting the autosuspend timer.
const IDLE_POLL: Duration = Duration::from_secs(5);

/// Backoff bookkeeping for NVIDIA polling.
#[derive(Default)]
struct IdleState {
    zero_streak: u32,
    has_other_procs: bool,
    last_poll: Option<Instant>,
    last: Option<NvData>,
}

/// Whether NVML should be queried now. While the GPU has been at 0% for `IDLE_SAMPLES` polls
/// and only the compositor uses it, the previous values are reused for `IDLE_POLL`.
pub fn should_poll_nvidia(
    zero_streak: u32,
    has_other_procs: bool,
    since_last: Option<Duration>,
) -> bool {
    match since_last {
        None => true,
        Some(el) => !(zero_streak >= IDLE_SAMPLES && !has_other_procs && el < IDLE_POLL),
    }
}

/// Display servers and compositors that always hold the GPU open.
pub fn is_compositor(name: &str) -> bool {
    matches!(
        name,
        "kwin_wayland"
            | "kwin_x11"
            | "Xorg"
            | "X"
            | "Xwayland"
            | "gnome-shell"
            | "mutter"
            | "sway"
            | "Hyprland"
            | "weston"
            | "labwc"
            | "wayfire"
            | "niri"
            | "river"
            | "cosmic-comp"
            | "plasmashell"
    )
}

fn pid_name(pid: u32) -> String {
    read_str(format!("/proc/{pid}/comm")).unwrap_or_else(|| format!("pid {pid}"))
}

impl NvBackend {
    fn drop_handle(&mut self) {
        self.nvml = None;
        self.smi_cache = None;
        self.idle = IdleState::default();
    }

    fn query(&mut self, pci: &str) -> Option<NvData> {
        let since = self.idle.last_poll.map(|t| t.elapsed());
        if !should_poll_nvidia(self.idle.zero_streak, self.idle.has_other_procs, since) {
            return self.idle.last.clone();
        }
        let data = self.query_now(pci);
        self.idle.last_poll = Some(Instant::now());
        match &data {
            Some(d) => {
                self.idle.zero_streak = if d.util == Some(0.0) {
                    self.idle.zero_streak + 1
                } else {
                    0
                };
                self.idle.has_other_procs = d
                    .procs
                    .iter()
                    .any(|(pid, _)| !is_compositor(&pid_name(*pid)));
            }
            None => {
                self.idle.zero_streak = 0;
                self.idle.has_other_procs = false;
            }
        }
        self.idle.last = data.clone();
        data
    }

    fn query_now(&mut self, pci: &str) -> Option<NvData> {
        if self.nvml.is_none() && !self.nvml_failed {
            match Nvml::init() {
                Ok(n) => self.nvml = Some(n),
                Err(_) => self.nvml_failed = true,
            }
        }
        if let Some(nvml) = &self.nvml
            && let Some(d) = Self::query_nvml(nvml, pci)
        {
            return Some(d);
        }
        // nvidia-smi spawns a process: rate-limit it.
        if let Some((at, data)) = &self.smi_cache
            && at.elapsed() < Duration::from_secs(3)
        {
            return data.clone();
        }
        let data = Self::query_smi(pci);
        self.smi_cache = Some((Instant::now(), data.clone()));
        data
    }

    fn query_nvml(nvml: &Nvml, pci: &str) -> Option<NvData> {
        let d = nvml.device_by_pci_bus_id(pci).ok()?;
        let mut out = NvData {
            name: d.name().ok(),
            ..Default::default()
        };
        if let Ok(u) = d.utilization_rates() {
            out.util = Some(u.gpu as f64);
        }
        if let Ok(m) = d.memory_info() {
            out.mem_used = Some(m.used);
            out.mem_total = Some(m.total);
        }
        out.temp = d.temperature(TemperatureSensor::Gpu).ok().map(|t| t as f64);
        out.power = d.power_usage().ok().map(|mw| mw as f64 / 1000.0);
        out.power_cap = d.enforced_power_limit().ok().map(|mw| mw as f64 / 1000.0);
        out.fan = d.fan_speed(0).ok().map(|p| p as f64);
        out.clk_gr = d.clock_info(Clock::Graphics).ok();
        out.clk_mem = d.clock_info(Clock::Memory).ok();
        out.pstate = d.performance_state().ok().map(|p| {
            let s = format!("{p:?}");
            // "Zero" .. "Fifteen" -> P0 .. P15
            const N: [&str; 16] = [
                "Zero", "One", "Two", "Three", "Four", "Five", "Six", "Seven", "Eight", "Nine",
                "Ten", "Eleven", "Twelve", "Thirteen", "Fourteen", "Fifteen",
            ];
            N.iter()
                .position(|n| *n == s)
                .map(|i| format!("P{i}"))
                .unwrap_or(s)
        });
        let mut procs: Vec<_> = d.running_graphics_processes().unwrap_or_default();
        procs.extend(d.running_compute_processes().unwrap_or_default());
        for p in procs {
            let vram = match p.used_gpu_memory {
                UsedGpuMemory::Used(b) => Some(b),
                UsedGpuMemory::Unavailable => None,
            };
            if !out.procs.iter().any(|(pid, _)| *pid == p.pid) {
                out.procs.push((p.pid, vram));
            }
        }
        Some(out)
    }

    fn query_smi(pci: &str) -> Option<NvData> {
        let out = std::process::Command::new("nvidia-smi")
            .args([
                "-i",
                pci,
                "--query-gpu=utilization.gpu,memory.used,memory.total,temperature.gpu,power.draw,power.limit,fan.speed,clocks.gr,clocks.mem,pstate",
                "--format=csv,noheader,nounits",
            ])
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        parse_smi_csv(String::from_utf8_lossy(&out.stdout).lines().next()?)
    }
}

// ---------------------------------------------------------------------------------------
// DRM fdinfo scanner
// ---------------------------------------------------------------------------------------

/// Per-process, per-GPU usage derived from fdinfo.
#[derive(Debug, Clone)]
struct ProcUse {
    pid: u32,
    pdev: String,
    pct: f64,
    vram: u64,
}

struct FdScanner {
    /// pid -> DRM file descriptors (found by readlink).
    fds: HashMap<u32, Vec<u32>>,
    last_full: Option<Instant>,
    prev: HashMap<(u32, String), u64>,
    prev_at: Option<Instant>,
}

impl FdScanner {
    fn new() -> Self {
        Self {
            fds: HashMap::new(),
            last_full: None,
            prev: HashMap::new(),
            prev_at: None,
        }
    }

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
                && t.starts_with("/dev/dri/")
            {
                v.push(fd);
            }
        }
        v
    }

    /// `skip_pdev`: devices that must not be reported (sleeping GPUs).
    fn scan(&mut self, skip_pdev: &[String]) -> Vec<ProcUse> {
        let now = Instant::now();
        let full = self
            .last_full
            .is_none_or(|t| now.duration_since(t) >= Duration::from_secs(10));
        if full {
            self.last_full = Some(now);
        }
        let mut pids: Vec<u32> = fs::read_dir("/proc")
            .map(|rd| {
                rd.flatten()
                    .filter_map(|e| e.file_name().to_string_lossy().parse().ok())
                    .collect()
            })
            .unwrap_or_default();
        pids.sort_unstable();
        let alive: std::collections::HashSet<u32> = pids.iter().copied().collect();
        self.fds.retain(|p, _| alive.contains(p));
        for &pid in &pids {
            if full || !self.fds.contains_key(&pid) {
                self.fds.insert(pid, Self::discover(pid));
            }
        }
        // (pdev, client) -> owning pid, first (lowest pid) wins.
        let mut seen: BTreeMap<(String, u64), ()> = BTreeMap::new();
        let mut totals: BTreeMap<(u32, String), (u64, u64)> = BTreeMap::new();
        for &pid in &pids {
            let Some(fds) = self.fds.get(&pid) else {
                continue;
            };
            for fd in fds {
                let Some(text) = read_str(format!("/proc/{pid}/fdinfo/{fd}")) else {
                    continue;
                };
                let Some(info) = parse_fdinfo(&text) else {
                    continue;
                };
                if skip_pdev.contains(&info.pdev)
                    || seen.insert((info.pdev.clone(), info.client), ()).is_some()
                {
                    continue;
                }
                let e = totals.entry((pid, info.pdev)).or_default();
                e.0 += info.engine_ns;
                e.1 += info.vram_bytes;
            }
        }
        let dt = self
            .prev_at
            .map(|t| now.duration_since(t).as_secs_f64())
            .unwrap_or(0.0)
            .max(0.001);
        let mut out = Vec::new();
        let mut prev = HashMap::new();
        for ((pid, pdev), (ns, vram)) in totals {
            let pct = match self.prev.get(&(pid, pdev.clone())) {
                Some(p) => (ns.saturating_sub(*p) as f64 / (dt * 1e9) * 100.0).clamp(0.0, 100.0),
                None => 0.0,
            };
            prev.insert((pid, pdev.clone()), ns);
            out.push(ProcUse {
                pid,
                pdev,
                pct,
                vram,
            });
        }
        self.prev = prev;
        self.prev_at = Some(now);
        out
    }
}

// ---------------------------------------------------------------------------------------
// Sampler
// ---------------------------------------------------------------------------------------

struct Dev {
    sysfs: PathBuf,
    hwmon: Option<PathBuf>,
    pci: String,
    vendor: Vendor,
    name: String,
    discrete: bool,
    nv: NvBackend,
}

pub struct GpuSampler {
    devs: Vec<Dev>,
    fdinfo: FdScanner,
}

fn pci_ids_name(vendor: u32, device: u32) -> Option<String> {
    for p in ["/usr/share/hwdata/pci.ids", "/usr/share/misc/pci.ids"] {
        if let Ok(t) = fs::read_to_string(p) {
            return pci_ids_lookup(&t, vendor, device);
        }
    }
    None
}

impl GpuSampler {
    pub fn new() -> Self {
        let mut cards: Vec<PathBuf> = fs::read_dir("/sys/class/drm")
            .map(|rd| {
                rd.flatten()
                    .filter(|e| {
                        let n = e.file_name().to_string_lossy().into_owned();
                        n.strip_prefix("card")
                            .is_some_and(|r| !r.is_empty() && r.chars().all(|c| c.is_ascii_digit()))
                    })
                    .map(|e| e.path().join("device"))
                    .collect()
            })
            .unwrap_or_default();
        cards.sort();
        let mut devs: Vec<Dev> = Vec::new();
        for dev in cards {
            let Some(vid) = read_hex(dev.join("vendor")) else {
                continue;
            };
            let did = read_hex(dev.join("device")).unwrap_or(0);
            let pci = fs::canonicalize(&dev)
                .ok()
                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                .unwrap_or_default();
            if devs.iter().any(|d| d.pci == pci) {
                continue;
            }
            let vendor = Vendor::from_id(vid);
            let boot_vga = read_u64(dev.join("boot_vga")) == Some(1);
            let discrete = match vendor {
                Vendor::Nvidia => true,
                Vendor::Intel => false,
                _ => !boot_vga,
            };
            let name = Self::friendly_name(vendor, vid, did);
            let hwmon = fs::read_dir(dev.join("hwmon"))
                .ok()
                .and_then(|rd| rd.flatten().map(|e| e.path()).min());
            devs.push(Dev {
                sysfs: dev,
                hwmon,
                pci,
                vendor,
                name,
                discrete,
                nv: NvBackend::default(),
            });
        }
        Self {
            devs,
            fdinfo: FdScanner::new(),
        }
    }

    fn friendly_name(vendor: Vendor, vid: u32, did: u32) -> String {
        // Phoenix iGPU (Radeon 780M/760M/740M share a device id); pci.ids only has the codename.
        if vid == 0x1002 && did == 0x15bf {
            return "AMD Radeon 780M".into();
        }
        match pci_ids_name(vid, did) {
            Some(n) if vendor == Vendor::Nvidia => format!("NVIDIA {n}"),
            Some(n) if vendor == Vendor::Amd => format!("AMD {n}"),
            Some(n) if vendor == Vendor::Intel => format!("Intel {n}"),
            Some(n) => n,
            None => format!("{} GPU", vendor.label()),
        }
    }

    pub fn sample(&mut self) -> Vec<Gpu> {
        let sleeping: Vec<String> = self
            .devs
            .iter()
            .filter(|d| is_sleeping(&d.sysfs))
            .map(|d| d.pci.clone())
            .collect();
        let uses = if self.devs.is_empty() {
            Vec::new()
        } else {
            self.fdinfo.scan(&sleeping)
        };
        let mut out = Vec::new();
        for (index, d) in self.devs.iter_mut().enumerate() {
            let mut g = Gpu {
                index,
                name: d.name.clone(),
                vendor: d.vendor,
                pci: d.pci.clone(),
                discrete: d.discrete,
                ..Default::default()
            };
            if sleeping.contains(&d.pci) {
                g.sleeping = true;
                d.nv.drop_handle();
                out.push(g);
                continue;
            }
            let mut nv_procs = Vec::new();
            match d.vendor {
                Vendor::Nvidia => nv_procs = Self::fill_nvidia(d, &mut g),
                _ => Self::fill_sysfs(d, &mut g),
            }
            for u in uses.iter().filter(|u| u.pdev == d.pci) {
                g.procs.push(GpuProc {
                    pid: u.pid,
                    name: String::new(),
                    gpu_pct: Some(u.pct),
                    vram: Some(u.vram),
                });
            }
            {
                // Add NVML-reported processes the fdinfo did not cover.
                for (pid, vram) in nv_procs {
                    match g.procs.iter_mut().find(|p| p.pid == pid) {
                        Some(p) => {
                            if p.vram.unwrap_or(0) == 0 {
                                p.vram = vram;
                            }
                        }
                        None => g.procs.push(GpuProc {
                            pid,
                            name: String::new(),
                            gpu_pct: None,
                            vram,
                        }),
                    }
                }
            }
            for p in &mut g.procs {
                p.name = pid_name(p.pid);
            }
            g.procs.sort_by(|a, b| {
                b.gpu_pct
                    .unwrap_or(0.0)
                    .total_cmp(&a.gpu_pct.unwrap_or(0.0))
                    .then(b.vram.unwrap_or(0).cmp(&a.vram.unwrap_or(0)))
                    .then(a.pid.cmp(&b.pid))
            });
            out.push(g);
        }
        out
    }

    fn fill_nvidia(d: &mut Dev, g: &mut Gpu) -> Vec<(u32, Option<u64>)> {
        let Some(nv) = d.nv.query(&d.pci) else {
            return Vec::new();
        };
        if let Some(n) = &nv.name {
            let n = n.trim();
            d.name = if n.starts_with("NVIDIA") {
                n.to_string()
            } else {
                format!("NVIDIA {n}")
            };
            g.name = d.name.clone();
        }
        g.util = nv.util;
        g.vram_used = nv.mem_used;
        g.vram_total = nv.mem_total;
        g.temp_c = nv.temp;
        g.power_w = nv.power;
        g.power_cap_w = nv.power_cap;
        g.fan_pct = nv.fan;
        g.clock_mhz = nv.clk_gr;
        g.mem_clock_mhz = nv.clk_mem;
        g.pstate = nv.pstate.clone();
        nv.procs
    }

    fn fill_sysfs(d: &Dev, g: &mut Gpu) {
        let s = &d.sysfs;
        g.util = read_u64(s.join("gpu_busy_percent")).map(|v| v as f64);
        g.vram_used = read_u64(s.join("mem_info_vram_used"));
        g.vram_total = read_u64(s.join("mem_info_vram_total"));
        g.gtt_used = read_u64(s.join("mem_info_gtt_used"));
        g.gtt_total = read_u64(s.join("mem_info_gtt_total"));
        g.clock_mhz = read_str(s.join("pp_dpm_sclk")).and_then(|t| parse_dpm_current(&t));
        g.mem_clock_mhz = read_str(s.join("pp_dpm_mclk")).and_then(|t| parse_dpm_current(&t));
        if let Some(h) = &d.hwmon {
            g.temp_c = read_u64(h.join("temp1_input")).map(|m| m as f64 / 1000.0);
            g.power_w = read_u64(h.join("power1_average"))
                .or_else(|| read_u64(h.join("power1_input")))
                .map(|u| u as f64 / 1e6);
            g.power_cap_w = read_u64(h.join("power1_cap")).map(|u| u as f64 / 1e6);
            g.fan_rpm = read_u64(h.join("fan1_input")).map(|r| r as u32);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AMD_FDINFO: &str = "pos:\t0\nflags:\t02100002\nmnt_id:\t26\nino:\t1234\n\
drm-driver:\tamdgpu\ndrm-client-name:\tradv\ndrm-pdev:\t0000:05:00.0\ndrm-client-id:\t42\n\
drm-engine-gfx:\t1500000 ns\ndrm-engine-compute:\t500000 ns\ndrm-engine-capacity-gfx:\t1\n\
drm-memory-vram:\t2048 KiB\ndrm-memory-gtt:\t100 KiB\ndrm-resident-vram:\t1024 KiB\n";

    #[test]
    fn runtime_status_detection() {
        let d = std::env::temp_dir().join(format!("mstop-gpu-{}", std::process::id()));
        std::fs::create_dir_all(d.join("power")).unwrap();
        std::fs::write(d.join("power/runtime_status"), "suspended\n").unwrap();
        assert!(is_sleeping(&d));
        std::fs::write(d.join("power/runtime_status"), "active\n").unwrap();
        assert!(!is_sleeping(&d));
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn nvidia_backoff_decision() {
        let s = Duration::from_secs;
        // First poll and busy GPUs always poll.
        assert!(should_poll_nvidia(0, false, None));
        assert!(should_poll_nvidia(2, false, Some(s(1))));
        // Idle for 5 samples and only the compositor: reuse values for 5 s.
        assert!(!should_poll_nvidia(5, false, Some(s(1))));
        assert!(!should_poll_nvidia(9, false, Some(s(4))));
        assert!(should_poll_nvidia(5, false, Some(s(5))));
        // A non-compositor process keeps 1 s polling.
        assert!(should_poll_nvidia(7, true, Some(s(1))));
        assert!(is_compositor("kwin_wayland") && !is_compositor("blender"));
    }

    #[test]
    fn fdinfo_amd() {
        let i = parse_fdinfo(AMD_FDINFO).unwrap();
        assert_eq!(i.pdev, "0000:05:00.0");
        assert_eq!(i.client, 42);
        assert_eq!(i.engine_ns, 2_000_000);
        assert_eq!(i.vram_bytes, 2048 * 1024);
    }

    #[test]
    fn fdinfo_resident_fallback_and_non_drm() {
        let t = "drm-pdev:\t0000:01:00.0\ndrm-client-id:\t7\ndrm-resident-vram:\t10 KiB\n";
        let i = parse_fdinfo(t).unwrap();
        assert_eq!(i.vram_bytes, 10 * 1024);
        assert_eq!(i.engine_ns, 0);
        assert!(parse_fdinfo("pos:\t0\nflags:\t0\n").is_none());
    }

    #[test]
    fn dpm_and_smi_and_pci_ids() {
        assert_eq!(
            parse_dpm_current("0: 800Mhz *\n1: 1100Mhz \n2: 2700Mhz \n"),
            Some(800)
        );
        assert_eq!(parse_dpm_current("0: 800Mhz\n"), None);
        let n = parse_smi_csv("12, 1024, 6141, 55, 14.50, 60.00, [N/A], 1500, 6001, P3").unwrap();
        assert_eq!(n.util, Some(12.0));
        assert_eq!(n.mem_used, Some(1024 * 1024 * 1024));
        assert_eq!(n.fan, None);
        assert_eq!(n.power_cap, Some(60.0));
        assert_eq!(n.pstate.as_deref(), Some("P3"));
        assert!(parse_smi_csv("garbage").is_none());
        let ids = "# c\n1002  AMD\n\t15bf  Phoenix1\n10de  NVIDIA Corporation\n\t28a0  AD107M\n\t28a1  AD107M [GeForce RTX 4050 Max-Q / Mobile]\n\t\t1043 1234  sub\n";
        assert_eq!(
            pci_ids_lookup(ids, 0x10de, 0x28a1).as_deref(),
            Some("GeForce RTX 4050 Max-Q / Mobile")
        );
        assert_eq!(
            pci_ids_lookup(ids, 0x1002, 0x15bf).as_deref(),
            Some("Phoenix1")
        );
        assert_eq!(pci_ids_lookup(ids, 0x1002, 0x28a1), None);
    }
}
