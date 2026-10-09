//! Sensors: hwmon temperatures and fans, CPU package power (RAPL), battery, power profile.
//!
//! Chips whose PCI/ACPI parent is runtime-suspended are skipped so reading them can never
//! wake a sleeping device.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

#[derive(Debug, Clone, Default)]
pub struct TempSensor {
    pub label: String,
    pub celsius: f64,
    pub max: Option<f64>,
    pub crit: Option<f64>,
}

#[derive(Debug, Clone, Default)]
pub struct Fan {
    pub label: String,
    pub rpm: u32,
}

#[derive(Debug, Clone, Default)]
pub struct Chip {
    /// Raw hwmon `name`.
    pub name: String,
    /// Friendly group title.
    pub title: String,
    pub temps: Vec<TempSensor>,
    pub fans: Vec<Fan>,
}

#[derive(Debug, Clone, Default)]
pub struct Sensors {
    pub chips: Vec<Chip>,
    /// CPU package temperature (k10temp Tctl / coretemp Package id 0).
    pub cpu_temp: Option<f64>,
    /// CPU package power in watts; None when unreadable (usually root-only).
    pub cpu_power_w: Option<f64>,
}

#[derive(Debug, Clone, Default)]
pub struct Battery {
    pub capacity: f64,
    pub status: String,
    /// Charge/discharge rate in watts.
    pub rate_w: Option<f64>,
    /// Energy in Wh.
    pub energy_now: Option<f64>,
    pub energy_full: Option<f64>,
    pub energy_design: Option<f64>,
    pub cycles: Option<u64>,
    pub time_left_secs: Option<u64>,
    pub ac_online: Option<bool>,
}

impl Battery {
    pub fn health_pct(&self) -> Option<f64> {
        match (self.energy_full, self.energy_design) {
            (Some(f), Some(d)) if d > 0.0 => Some(f / d * 100.0),
            _ => None,
        }
    }
    pub fn discharging(&self) -> bool {
        self.status.eq_ignore_ascii_case("discharging")
    }
    pub fn charging(&self) -> bool {
        self.status.eq_ignore_ascii_case("charging")
    }
}

pub fn friendly_chip(name: &str) -> String {
    match name {
        "k10temp" | "coretemp" | "zenpower" => "CPU".into(),
        "amdgpu" => "Radeon iGPU".into(),
        "nouveau" | "nvidia" => "NVIDIA GPU".into(),
        "nvme" => "NVMe SSD".into(),
        "asus" | "asus_wmi_sensors" | "asus-nb-wmi" => "ASUS fans".into(),
        "acpitz" => "Motherboard".into(),
        "iwlwifi" | "iwlwifi_1" | "mt7921_phy0" | "mt7921e" | "ath11k_hwmon" | "ath10k_hwmon" => {
            "Wi-Fi".into()
        }
        "spd5118" | "jc42" => "RAM".into(),
        other => other.to_string(),
    }
}

fn read_str(p: impl AsRef<Path>) -> Option<String> {
    fs::read_to_string(p).ok().map(|s| s.trim().to_string())
}

fn read_f64(p: impl AsRef<Path>) -> Option<f64> {
    read_str(p)?.parse().ok()
}

fn milli(p: impl AsRef<Path>) -> Option<f64> {
    read_f64(p).map(|m| m / 1000.0)
}

/// Limits outside a sane range are firmware junk (e.g. 65261 °C).
fn sane_limit(v: Option<f64>) -> Option<f64> {
    v.filter(|v| (30.0..=150.0).contains(v))
}

fn sorted_files(dir: &Path, prefix: &str, suffix: &str) -> Vec<(u32, String)> {
    let mut v: Vec<(u32, String)> = fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| {
                    let n = e.file_name().to_string_lossy().into_owned();
                    let num = n.strip_prefix(prefix)?.strip_suffix(suffix)?;
                    Some((num.parse().ok()?, n))
                })
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

/// True when the hwmon's parent device is runtime-suspended (reading would wake it).
fn parent_sleeping(hwmon: &Path) -> bool {
    matches!(
        read_str(hwmon.join("device/power/runtime_status")).as_deref(),
        Some("suspended") | Some("suspending")
    )
}

/// Read every `hwmon*` below `root` (normally /sys/class/hwmon).
pub fn read_hwmon(root: &Path) -> Vec<Chip> {
    let mut dirs: Vec<PathBuf> = fs::read_dir(root)
        .map(|rd| rd.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    dirs.sort_by_key(|p| {
        let n = p.file_name().map(|n| n.to_string_lossy().into_owned());
        n.and_then(|n| n.strip_prefix("hwmon").and_then(|x| x.parse::<u32>().ok()))
            .unwrap_or(u32::MAX)
    });
    let mut chips = Vec::new();
    for dir in dirs {
        let Some(name) = read_str(dir.join("name")) else {
            continue;
        };
        if parent_sleeping(&dir) {
            continue;
        }
        let mut chip = Chip {
            title: friendly_chip(&name),
            name,
            ..Default::default()
        };
        for (n, file) in sorted_files(&dir, "temp", "_input") {
            let Some(celsius) = milli(dir.join(&file)).filter(|c| *c > -100.0) else {
                continue;
            };
            let label = read_str(dir.join(format!("temp{n}_label")))
                .unwrap_or_else(|| format!("Sensor {n}"));
            chip.temps.push(TempSensor {
                label,
                celsius,
                max: sane_limit(milli(dir.join(format!("temp{n}_max")))),
                crit: sane_limit(milli(dir.join(format!("temp{n}_crit")))),
            });
        }
        for (n, file) in sorted_files(&dir, "fan", "_input") {
            let Some(rpm) = read_f64(dir.join(&file)) else {
                continue;
            };
            let label =
                read_str(dir.join(format!("fan{n}_label"))).unwrap_or_else(|| format!("Fan {n}"));
            chip.fans.push(Fan {
                label,
                rpm: rpm as u32,
            });
        }
        if !chip.temps.is_empty() || !chip.fans.is_empty() {
            chips.push(chip);
        }
    }
    chips
}

/// CPU package temperature out of the chip list.
pub fn cpu_package_temp(chips: &[Chip]) -> Option<f64> {
    for c in chips {
        if c.name == "k10temp" {
            if let Some(t) = c.temps.iter().find(|t| t.label == "Tctl") {
                return Some(t.celsius);
            }
        } else if c.name == "coretemp"
            && let Some(t) = c.temps.iter().find(|t| t.label.starts_with("Package id 0"))
        {
            return Some(t.celsius);
        }
    }
    chips
        .iter()
        .find(|c| c.name == "k10temp")
        .and_then(|c| c.temps.first())
        .map(|t| t.celsius)
}

/// Watts from two RAPL energy readings (µJ), handling counter wrap.
pub fn rapl_watts(prev_uj: u64, cur_uj: u64, max_range_uj: Option<u64>, secs: f64) -> Option<f64> {
    if secs <= 0.0 {
        return None;
    }
    let delta = if cur_uj >= prev_uj {
        cur_uj - prev_uj
    } else {
        max_range_uj?.checked_sub(prev_uj)? + cur_uj
    };
    Some(delta as f64 / 1e6 / secs)
}

/// Energy in Wh from `energy_*` (µWh) or, failing that, `charge_*` (µAh) × voltage.
fn battery_energy(dir: &Path, which: &str, volt_v: Option<f64>) -> Option<f64> {
    if let Some(e) = read_f64(dir.join(format!("energy_{which}"))) {
        return Some(e / 1e6);
    }
    let c = read_f64(dir.join(format!("charge_{which}")))?;
    Some(c / 1e6 * volt_v?)
}

pub fn read_battery(root: &Path) -> Option<Battery> {
    let mut bats: Vec<PathBuf> = fs::read_dir(root)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("BAT"))
        })
        .collect();
    bats.sort();
    let dir = bats.into_iter().next()?;
    let volt = read_f64(dir.join("voltage_now"))
        .or_else(|| read_f64(dir.join("voltage_min_design")))
        .map(|v| v / 1e6);
    let energy_now = battery_energy(&dir, "now", volt);
    let energy_full = battery_energy(&dir, "full", volt);
    let energy_design = battery_energy(&dir, "full_design", volt);
    let capacity =
        read_f64(dir.join("capacity")).unwrap_or_else(|| match (energy_now, energy_full) {
            (Some(n), Some(f)) if f > 0.0 => n / f * 100.0,
            _ => 0.0,
        });
    let status = read_str(dir.join("status")).unwrap_or_else(|| "Unknown".into());
    let rate_w = read_f64(dir.join("power_now"))
        .map(|p| p / 1e6)
        .or_else(|| {
            let i = read_f64(dir.join("current_now"))?;
            Some(i * read_f64(dir.join("voltage_now"))? / 1e12)
        })
        .map(f64::abs);
    let mut b = Battery {
        capacity,
        status,
        rate_w,
        energy_now,
        energy_full,
        energy_design,
        cycles: read_f64(dir.join("cycle_count")).map(|c| c as u64),
        time_left_secs: None,
        ac_online: read_ac(root),
    };
    b.time_left_secs = battery_time_left(&b);
    Some(b)
}

pub fn battery_time_left(b: &Battery) -> Option<u64> {
    let rate = b.rate_w.filter(|r| *r > 0.1)?;
    let hours = if b.discharging() {
        b.energy_now? / rate
    } else if b.charging() {
        (b.energy_full? - b.energy_now?).max(0.0) / rate
    } else {
        return None;
    };
    Some((hours * 3600.0) as u64)
}

/// True when any mains-type supply (`type` == Mains) is online.
pub fn read_ac(root: &Path) -> Option<bool> {
    let mut found = None;
    for e in fs::read_dir(root).ok()?.flatten() {
        let p = e.path();
        if read_str(p.join("type")).as_deref() == Some("Mains")
            && let Some(on) = read_str(p.join("online"))
        {
            found = Some(found.unwrap_or(false) || on == "1");
        }
    }
    found
}

pub struct SensorSampler {
    hwmon_root: PathBuf,
    ps_root: PathBuf,
    rapl: PathBuf,
    rapl_max: Option<u64>,
    prev_energy: Option<(u64, Instant)>,
}

impl SensorSampler {
    pub fn new() -> Self {
        let rapl = PathBuf::from("/sys/class/powercap/intel-rapl:0");
        Self {
            hwmon_root: "/sys/class/hwmon".into(),
            ps_root: "/sys/class/power_supply".into(),
            rapl_max: read_f64(rapl.join("max_energy_range_uj")).map(|v| v as u64),
            rapl,
            prev_energy: None,
        }
    }

    fn package_power(&mut self) -> Option<f64> {
        let cur = read_f64(self.rapl.join("energy_uj"))? as u64;
        let now = Instant::now();
        let prev = self.prev_energy.replace((cur, now));
        let (p, at) = prev?;
        rapl_watts(p, cur, self.rapl_max, now.duration_since(at).as_secs_f64())
    }

    pub fn sample(&mut self) -> (Sensors, Option<Battery>, Option<String>) {
        let chips = read_hwmon(&self.hwmon_root);
        let cpu_temp = cpu_package_temp(&chips);
        let s = Sensors {
            chips,
            cpu_temp,
            cpu_power_w: self.package_power(),
        };
        let profile = read_str("/sys/firmware/acpi/platform_profile");
        (s, read_battery(&self.ps_root), profile)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(dir: &Path, rel: &str, content: &str) {
        let p = dir.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mstop-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn hwmon_parsing_and_grouping() {
        let d = tmp("hwmon");
        w(&d, "hwmon0/name", "k10temp\n");
        w(&d, "hwmon0/temp1_input", "61250\n");
        w(&d, "hwmon0/temp1_label", "Tctl\n");
        w(&d, "hwmon1/name", "nvme\n");
        w(&d, "hwmon1/temp1_input", "40850\n");
        w(&d, "hwmon1/temp1_label", "Composite\n");
        w(&d, "hwmon1/temp1_max", "81850\n");
        w(&d, "hwmon1/temp1_crit", "84850\n");
        w(&d, "hwmon1/temp2_input", "41000\n");
        w(&d, "hwmon1/temp2_max", "65261850\n");
        w(&d, "hwmon2/name", "asus\n");
        w(&d, "hwmon2/fan1_input", "2400\n");
        w(&d, "hwmon2/fan1_label", "cpu_fan\n");
        w(&d, "hwmon3/name", "BAT1\n");
        w(&d, "hwmon3/in0_input", "12000\n");
        w(&d, "hwmon4/name", "amdgpu\n");
        w(&d, "hwmon4/temp1_input", "50000\n");
        w(&d, "hwmon4/device/power/runtime_status", "suspended\n");
        let chips = read_hwmon(&d);
        let titles: Vec<_> = chips.iter().map(|c| c.title.as_str()).collect();
        assert_eq!(titles, ["CPU", "NVMe SSD", "ASUS fans"]);
        assert_eq!(chips[0].temps[0].celsius, 61.25);
        assert_eq!(chips[1].temps[0].max, Some(81.85));
        assert_eq!(chips[1].temps[0].crit, Some(84.85));
        assert_eq!(chips[1].temps[1].label, "Sensor 2");
        assert_eq!(chips[1].temps[1].max, None);
        assert_eq!(chips[2].fans[0].rpm, 2400);
        assert_eq!(chips[2].fans[0].label, "cpu_fan");
        assert_eq!(cpu_package_temp(&chips), Some(61.25));
        assert_eq!(friendly_chip("mystery"), "mystery");
        let _ = fs::remove_dir_all(d);
    }

    #[test]
    fn rapl_math() {
        assert_eq!(rapl_watts(1_000_000, 11_000_000, None, 2.0), Some(5.0));
        assert_eq!(rapl_watts(900, 100, Some(1000), 1.0), Some(200.0 / 1e6));
        assert_eq!(rapl_watts(900, 100, None, 1.0), None);
        assert_eq!(rapl_watts(1, 2, None, 0.0), None);
    }

    #[test]
    fn battery_parsing() {
        let d = tmp("bat");
        w(&d, "BAT1/type", "Battery\n");
        w(&d, "BAT1/capacity", "92\n");
        w(&d, "BAT1/status", "Discharging\n");
        w(&d, "BAT1/power_now", "14200000\n");
        w(&d, "BAT1/energy_now", "45000000\n");
        w(&d, "BAT1/energy_full", "48000000\n");
        w(&d, "BAT1/energy_full_design", "50000000\n");
        w(&d, "BAT1/cycle_count", "42\n");
        w(&d, "ACAD/type", "Mains\n");
        w(&d, "ACAD/online", "0\n");
        let b = read_battery(&d).unwrap();
        assert_eq!(b.capacity, 92.0);
        assert_eq!(b.rate_w, Some(14.2));
        assert_eq!(b.cycles, Some(42));
        assert_eq!(b.ac_online, Some(false));
        assert!((b.health_pct().unwrap() - 96.0).abs() < 1e-9);
        // 45 Wh / 14.2 W = 3.169 h
        assert_eq!(b.time_left_secs, Some((45.0 / 14.2 * 3600.0) as u64));
        // Charging: time to full from energy delta.
        w(&d, "BAT1/status", "Charging\n");
        w(&d, "ACAD/online", "1\n");
        let b = read_battery(&d).unwrap();
        assert_eq!(b.ac_online, Some(true));
        assert_eq!(b.time_left_secs, Some((3.0 / 14.2 * 3600.0) as u64));
        // Charge-based fallback with current*voltage.
        let d2 = tmp("bat2");
        w(&d2, "BAT0/capacity", "50\n");
        w(&d2, "BAT0/status", "Discharging\n");
        w(&d2, "BAT0/current_now", "1000000\n");
        w(&d2, "BAT0/voltage_now", "12000000\n");
        w(&d2, "BAT0/charge_now", "2000000\n");
        w(&d2, "BAT0/charge_full", "4000000\n");
        let b = read_battery(&d2).unwrap();
        assert_eq!(b.rate_w, Some(12.0));
        assert_eq!(b.energy_now, Some(24.0));
        assert_eq!(b.time_left_secs, Some(2 * 3600));
        assert!(read_battery(&tmp("nobat")).is_none());
        let _ = fs::remove_dir_all(d);
        let _ = fs::remove_dir_all(d2);
    }
}
