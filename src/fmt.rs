//! Human-readable formatting helpers.

fn scaled(value: f64, units: &[&str], base: f64) -> String {
    let mut v = value.max(0.0);
    let mut i = 0;
    while v >= base && i + 1 < units.len() {
        v /= base;
        i += 1;
    }
    if i == 0 {
        format!("{} {}", v as u64, units[0])
    } else if v >= 99.95 {
        format!("{:.0} {}", v, units[i])
    } else {
        format!("{:.1} {}", v, units[i])
    }
}

/// Binary byte size: `11.2 GiB`, `840 MiB`.
pub fn bytes(n: u64) -> String {
    scaled(n as f64, &["B", "KiB", "MiB", "GiB", "TiB", "PiB"], 1024.0)
}

/// Transfer rate in decimal units: `42.4 MB/s`.
pub fn rate(bytes_per_sec: f64) -> String {
    scaled(
        bytes_per_sec,
        &["B/s", "KB/s", "MB/s", "GB/s", "TB/s"],
        1000.0,
    )
}

fn plural(n: u64, unit: &str) -> String {
    if n == 1 {
        format!("{n} {unit}")
    } else {
        format!("{n} {unit}s")
    }
}

/// Duration: `4 days, 16 hours, 22 minutes`.
pub fn duration(secs: u64) -> String {
    let d = secs / 86_400;
    let h = secs % 86_400 / 3600;
    let m = secs % 3600 / 60;
    let mut parts = Vec::new();
    if d > 0 {
        parts.push(plural(d, "day"));
    }
    if h > 0 {
        parts.push(plural(h, "hour"));
    }
    if m > 0 {
        parts.push(plural(m, "minute"));
    }
    if parts.is_empty() {
        return plural(secs % 60, "second");
    }
    parts.join(", ")
}

/// Percentage with one decimal: `35.8%`.
pub fn pct(p: f64) -> String {
    format!("{p:.1}%")
}

/// Frequency given in MHz: `3.8 GHz`.
pub fn ghz(mhz: f64) -> String {
    format!("{:.1} GHz", mhz / 1000.0)
}

/// Integer with thousands separators: `1,203`.
pub fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_bytes() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(840 * 1024 * 1024), "840 MiB");
        assert_eq!(bytes((11.2 * 1024.0 * 1024.0 * 1024.0) as u64), "11.2 GiB");
        assert_eq!(bytes(1536), "1.5 KiB");
    }

    #[test]
    fn formats_rates() {
        assert_eq!(rate(42_400_000.0), "42.4 MB/s");
        assert_eq!(rate(0.0), "0 B/s");
        assert_eq!(rate(1500.0), "1.5 KB/s");
    }

    #[test]
    fn formats_duration() {
        assert_eq!(
            duration(4 * 86_400 + 16 * 3600 + 22 * 60),
            "4 days, 16 hours, 22 minutes"
        );
        assert_eq!(duration(3600 + 300), "1 hour, 5 minutes");
        assert_eq!(duration(42), "42 seconds");
    }

    #[test]
    fn formats_misc() {
        assert_eq!(pct(35.84), "35.8%");
        assert_eq!(ghz(3800.0), "3.8 GHz");
        assert_eq!(thousands(1203), "1,203");
        assert_eq!(thousands(12), "12");
        assert_eq!(thousands(1_234_567), "1,234,567");
    }
}
