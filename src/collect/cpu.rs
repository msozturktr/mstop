//! CPU sampler: usage, frequencies, governor, load, uptime.

use std::fs;
use sysinfo::System;

#[derive(Debug, Clone, Default)]
pub struct Core {
    pub usage: f64,
    pub freq_mhz: Option<f64>,
}

#[derive(Debug, Clone, Default)]
pub struct CpuInfo {
    pub model: String,
    pub physical_cores: usize,
    pub threads: usize,
    pub total: f64,
    pub cores: Vec<Core>,
    pub max_mhz: Option<f64>,
    pub governor: Option<String>,
    pub load: [f64; 3],
    pub uptime_secs: u64,
}

impl CpuInfo {
    pub fn avg_freq_mhz(&self) -> Option<f64> {
        let f: Vec<f64> = self.cores.iter().filter_map(|c| c.freq_mhz).collect();
        if f.is_empty() {
            None
        } else {
            Some(f.iter().sum::<f64>() / f.len() as f64)
        }
    }
}

pub struct CpuSampler {
    model: String,
    physical: usize,
    max_mhz: Option<f64>,
}

fn read_khz(path: &str) -> Option<f64> {
    fs::read_to_string(path)
        .ok()?
        .trim()
        .parse::<f64>()
        .ok()
        .map(|k| k / 1000.0)
}

impl CpuSampler {
    pub fn new(sys: &System) -> Self {
        let model = sys
            .cpus()
            .first()
            .map(|c| c.brand().trim().to_string())
            .unwrap_or_else(|| "Unknown CPU".into());
        let max_mhz = (0..sys.cpus().len())
            .filter_map(|i| {
                read_khz(&format!(
                    "/sys/devices/system/cpu/cpu{i}/cpufreq/cpuinfo_max_freq"
                ))
            })
            .reduce(f64::max);
        Self {
            model,
            physical: System::physical_core_count().unwrap_or(sys.cpus().len()),
            max_mhz,
        }
    }

    pub fn sample(&mut self, sys: &System) -> CpuInfo {
        let cores = sys
            .cpus()
            .iter()
            .enumerate()
            .map(|(i, c)| Core {
                usage: c.cpu_usage() as f64,
                freq_mhz: read_khz(&format!(
                    "/sys/devices/system/cpu/cpu{i}/cpufreq/scaling_cur_freq"
                ))
                .or_else(|| (c.frequency() > 0).then(|| c.frequency() as f64)),
            })
            .collect::<Vec<_>>();
        let la = System::load_average();
        CpuInfo {
            model: self.model.clone(),
            physical_cores: self.physical,
            threads: cores.len(),
            total: sys.global_cpu_usage() as f64,
            cores,
            max_mhz: self.max_mhz,
            governor: fs::read_to_string("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor")
                .ok()
                .map(|s| s.trim().to_string()),
            load: [la.one, la.five, la.fifteen],
            uptime_secs: System::uptime(),
        }
    }
}
