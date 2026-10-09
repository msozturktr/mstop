//! Memory sampler: /proc/meminfo and ZRAM devices.

use std::collections::HashMap;
use std::fs;

#[derive(Debug, Clone, Default)]
pub struct Zram {
    pub name: String,
    pub disksize: Option<u64>,
    pub orig: Option<u64>,
    pub compr: Option<u64>,
}

impl Zram {
    pub fn ratio(&self) -> Option<f64> {
        match (self.orig, self.compr) {
            (Some(o), Some(c)) if c > 0 => Some(o as f64 / c as f64),
            _ => None,
        }
    }
    pub fn saved(&self) -> Option<u64> {
        Some(self.orig?.saturating_sub(self.compr?))
    }
}

#[derive(Debug, Clone, Default)]
pub struct MemInfo {
    pub total: u64,
    pub used: u64,
    pub available: u64,
    pub cache: u64,
    pub shared: u64,
    pub dirty: u64,
    pub slab: u64,
    pub swap_total: u64,
    pub swap_used: u64,
    pub zram: Vec<Zram>,
}

impl MemInfo {
    pub fn free(&self) -> u64 {
        self.total.saturating_sub(self.used + self.cache)
    }
}

/// Parse the text of /proc/meminfo. Returns None when MemTotal is missing.
pub fn parse_meminfo(text: &str) -> Option<MemInfo> {
    let mut m: HashMap<&str, u64> = HashMap::new();
    for line in text.lines() {
        let Some((k, rest)) = line.split_once(':') else {
            continue;
        };
        let mut it = rest.split_whitespace();
        let Some(v) = it.next().and_then(|v| v.parse::<u64>().ok()) else {
            continue;
        };
        let mult = if it.next() == Some("kB") { 1024 } else { 1 };
        m.insert(k, v * mult);
    }
    let g = |k: &str| m.get(k).copied().unwrap_or(0);
    let total = *m.get("MemTotal")?;
    let available = m
        .get("MemAvailable")
        .copied()
        .unwrap_or_else(|| g("MemFree") + g("Cached") + g("Buffers"));
    Some(MemInfo {
        total,
        used: total.saturating_sub(available),
        available,
        cache: g("Cached") + g("Buffers") + g("SReclaimable"),
        shared: g("Shmem"),
        dirty: g("Dirty"),
        slab: g("Slab"),
        swap_total: g("SwapTotal"),
        swap_used: g("SwapTotal").saturating_sub(g("SwapFree")),
        zram: Vec::new(),
    })
}

/// Parse /sys/block/zramN/mm_stat: returns (orig_data_size, compr_data_size).
pub fn parse_mm_stat(text: &str) -> Option<(u64, u64)> {
    let mut it = text.split_whitespace();
    let orig = it.next()?.parse().ok()?;
    let compr = it.next()?.parse().ok()?;
    Some((orig, compr))
}

fn read_zram() -> Vec<Zram> {
    let Ok(dir) = fs::read_dir("/sys/block") else {
        return Vec::new();
    };
    let mut names: Vec<String> = dir
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("zram"))
        .collect();
    names.sort();
    names
        .into_iter()
        .map(|name| {
            let base = format!("/sys/block/{name}");
            let mm = fs::read_to_string(format!("{base}/mm_stat"))
                .ok()
                .and_then(|t| parse_mm_stat(&t));
            let disksize = fs::read_to_string(format!("{base}/disksize"))
                .ok()
                .and_then(|t| t.trim().parse().ok());
            Zram {
                name,
                disksize,
                orig: mm.map(|x| x.0),
                compr: mm.map(|x| x.1),
            }
        })
        .collect()
}

#[derive(Default)]
pub struct MemSampler;

impl MemSampler {
    pub fn sample(&mut self) -> MemInfo {
        let mut info = fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|t| parse_meminfo(&t))
            .unwrap_or_default();
        info.zram = read_zram();
        info
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = "MemTotal:       32768000 kB
MemFree:         4000000 kB
MemAvailable:   20000000 kB
Buffers:          100000 kB
Cached:          8000000 kB
SwapCached:            0 kB
Dirty:              2048 kB
Shmem:           1000000 kB
Slab:             500000 kB
SReclaimable:     300000 kB
SwapTotal:       8192000 kB
SwapFree:        8000000 kB
";

    #[test]
    fn parses_meminfo() {
        let m = parse_meminfo(FIXTURE).unwrap();
        assert_eq!(m.total, 32_768_000 * 1024);
        assert_eq!(m.available, 20_000_000 * 1024);
        assert_eq!(m.used, 12_768_000 * 1024);
        assert_eq!(m.cache, (8_000_000 + 100_000 + 300_000) * 1024);
        assert_eq!(m.shared, 1_000_000 * 1024);
        assert_eq!(m.dirty, 2048 * 1024);
        assert_eq!(m.slab, 500_000 * 1024);
        assert_eq!(m.swap_used, 192_000 * 1024);
    }

    #[test]
    fn meminfo_missing_total_is_none() {
        assert!(parse_meminfo("MemFree: 1 kB\n").is_none());
        assert!(parse_meminfo("").is_none());
    }

    #[test]
    fn parses_mm_stat() {
        let t = "4096000 1024000 1100000 0 1100000 0 0 0 0\n";
        assert_eq!(parse_mm_stat(t), Some((4_096_000, 1_024_000)));
        assert_eq!(parse_mm_stat("garbage"), None);
        let z = Zram {
            name: "zram0".into(),
            disksize: None,
            orig: Some(4000),
            compr: Some(1000),
        };
        assert_eq!(z.ratio(), Some(4.0));
        assert_eq!(z.saved(), Some(3000));
    }
}
