//! Fixed-capacity ring buffer of samples.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

pub const CAPACITY: usize = 120;

#[derive(Debug, Clone)]
pub struct History<T = f64> {
    buf: VecDeque<T>,
    cap: usize,
}

impl<T: Copy + Into<f64>> History<T> {
    pub fn new(cap: usize) -> Self {
        Self {
            buf: VecDeque::with_capacity(cap),
            cap: cap.max(1),
        }
    }

    pub fn push(&mut self, v: T) {
        if self.buf.len() == self.cap {
            self.buf.pop_front();
        }
        self.buf.push_back(v);
    }

    pub fn iter(&self) -> impl Iterator<Item = T> + '_ {
        self.buf.iter().copied()
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn min(&self) -> Option<f64> {
        self.iter().map(Into::into).reduce(f64::min)
    }

    pub fn max(&self) -> Option<f64> {
        self.iter().map(Into::into).reduce(f64::max)
    }

    pub fn avg(&self) -> Option<f64> {
        if self.buf.is_empty() {
            None
        } else {
            Some(self.iter().map(Into::into).sum::<f64>() / self.buf.len() as f64)
        }
    }

    /// The most recent `n` samples (oldest first) as f64.
    pub fn tail(&self, n: usize) -> Vec<f64> {
        let skip = self.buf.len().saturating_sub(n);
        self.buf.iter().skip(skip).map(|v| (*v).into()).collect()
    }
}

impl<T: Copy + Into<f64>> Default for History<T> {
    fn default() -> Self {
        Self::new(CAPACITY)
    }
}

/// Samples kept at the refresh rate: 10 minutes at the default 1 s interval.
pub const SHORT_CAP: usize = 600;
/// Per-minute averages kept for 24 hours.
pub const LONG_CAP: usize = 1440;
/// Points drawn in a graph, whatever the span.
pub const VIEW_POINTS: usize = 60;

/// Time span shown by every graph.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Span {
    #[default]
    S60,
    M10,
    H1,
    H24,
}

impl Span {
    pub fn next(self) -> Self {
        match self {
            Span::S60 => Span::M10,
            Span::M10 => Span::H1,
            Span::H1 => Span::H24,
            Span::H24 => Span::S60,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Span::S60 => "60 s",
            Span::M10 => "10 min",
            Span::H1 => "1 h",
            Span::H24 => "24 h",
        }
    }
    /// Minutes covered by the per-minute tier, or None for the live tier.
    fn minutes(self) -> Option<u64> {
        match self {
            Span::H1 => Some(60),
            Span::H24 => Some(LONG_CAP as u64),
            _ => None,
        }
    }
}

/// Wall-clock minutes since the Unix epoch.
pub fn now_minute() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() / 60)
}

/// A live ring plus a per-minute average tier.
#[derive(Debug, Clone)]
pub struct Tiered {
    short: History,
    long: VecDeque<(u64, f64)>,
    acc_sum: f64,
    acc_n: u32,
    acc_min: u64,
}

impl Default for Tiered {
    fn default() -> Self {
        Self {
            short: History::new(SHORT_CAP),
            long: VecDeque::new(),
            acc_sum: 0.0,
            acc_n: 0,
            acc_min: 0,
        }
    }
}

impl Tiered {
    pub fn push(&mut self, v: f64) {
        self.push_at(v, now_minute());
    }

    pub fn push_at(&mut self, v: f64, minute: u64) {
        self.short.push(v);
        if self.acc_n > 0 && minute != self.acc_min {
            self.flush();
        }
        self.acc_min = minute;
        self.acc_sum += v;
        self.acc_n += 1;
    }

    fn flush(&mut self) {
        if self.acc_n > 0 {
            if self.long.len() == LONG_CAP {
                self.long.pop_front();
            }
            self.long
                .push_back((self.acc_min, self.acc_sum / self.acc_n as f64));
        }
        self.acc_sum = 0.0;
        self.acc_n = 0;
    }

    pub fn min(&self) -> Option<f64> {
        self.short.min()
    }
    pub fn max(&self) -> Option<f64> {
        self.short.max()
    }
    pub fn avg(&self) -> Option<f64> {
        self.short.avg()
    }

    /// Completed per-minute points, oldest first.
    pub fn minutes(&self) -> &VecDeque<(u64, f64)> {
        &self.long
    }

    /// Replace the per-minute tier with points loaded from disk (older than anything live).
    pub fn restore(&mut self, points: Vec<(u64, f64)>) {
        let first_live = self.long.front().map(|p| p.0).unwrap_or(self.acc_min);
        let live_started = self.acc_n > 0 || !self.long.is_empty();
        let mut merged: VecDeque<(u64, f64)> = points
            .into_iter()
            .filter(|p| !live_started || p.0 < first_live)
            .collect();
        merged.extend(self.long.drain(..));
        while merged.len() > LONG_CAP {
            merged.pop_front();
        }
        self.long = merged;
    }

    /// Up to `VIEW_POINTS` values (oldest first) covering `span`.
    pub fn view(&self, span: Span, interval_ms: u64) -> Vec<f64> {
        self.view_at(span, interval_ms, now_minute())
    }

    pub fn view_at(&self, span: Span, interval_ms: u64, now: u64) -> Vec<f64> {
        if let Some(mins) = span.minutes() {
            return self.long_view(mins, now);
        }
        if span == Span::S60 {
            return self.short.tail(VIEW_POINTS);
        }
        let want = (600_000 / interval_ms.max(1)).max(1) as usize;
        let v = self.short.tail(want);
        if v.len() <= VIEW_POINTS {
            return v;
        }
        (0..VIEW_POINTS)
            .map(|b| {
                let (lo, hi) = (b * v.len() / VIEW_POINTS, (b + 1) * v.len() / VIEW_POINTS);
                let c = &v[lo..hi.max(lo + 1)];
                c.iter().sum::<f64>() / c.len() as f64
            })
            .collect()
    }

    fn long_view(&self, minutes: u64, now: u64) -> Vec<f64> {
        let start = (now + 1).saturating_sub(minutes);
        let mut sum = [0.0f64; VIEW_POINTS];
        let mut cnt = [0u32; VIEW_POINTS];
        let partial = (self.acc_n > 0).then(|| (self.acc_min, self.acc_sum / self.acc_n as f64));
        for (m, v) in self.long.iter().copied().chain(partial) {
            if m < start || m > now {
                continue;
            }
            let b = ((m - start) * VIEW_POINTS as u64 / minutes) as usize;
            sum[b.min(VIEW_POINTS - 1)] += v;
            cnt[b.min(VIEW_POINTS - 1)] += 1;
        }
        let Some(first) = cnt.iter().position(|c| *c > 0) else {
            return Vec::new();
        };
        let mut last = 0.0;
        (first..VIEW_POINTS)
            .map(|b| {
                if cnt[b] > 0 {
                    last = sum[b] / cnt[b] as f64;
                }
                last
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------------------
// Persistence: a small versioned text file, written atomically.
// ---------------------------------------------------------------------------------------

const MAGIC: &str = "mstop-history";
const VERSION: u32 = 1;

/// `$XDG_STATE_HOME/mstop/history`, falling back to `~/.local/state/mstop/history`.
pub fn state_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|v| !v.is_empty())
                .map(|h| PathBuf::from(h).join(".local/state"))
        })?;
    Some(base.join("mstop/history"))
}

/// Write all series to `path` via a temp file and rename.
pub fn save(path: &Path, series: &[(String, &Tiered)]) -> std::io::Result<()> {
    use std::fmt::Write as _;
    let mut out = format!("{MAGIC} {VERSION}\n");
    for (name, t) in series {
        let pts: Vec<_> = t.minutes().iter().filter(|p| p.1.is_finite()).collect();
        let _ = writeln!(out, "s {name} {}", pts.len());
        let line: Vec<String> = pts.iter().map(|(m, v)| format!("{m}:{v}")).collect();
        let _ = writeln!(out, "{}", line.join(","));
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, out)?;
    std::fs::rename(&tmp, path)
}

/// Read series from `path`, dropping points older than 24 h. A missing, corrupt or
/// differently versioned file yields nothing.
pub fn load(path: &Path, now: u64) -> Vec<(String, Vec<(u64, f64)>)> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut lines = text.lines();
    let ok = lines
        .next()
        .and_then(|h| h.strip_prefix(MAGIC))
        .and_then(|v| v.trim().parse::<u32>().ok())
        == Some(VERSION);
    if !ok {
        return Vec::new();
    }
    let mut out = Vec::new();
    while let Some(head) = lines.next() {
        let Some(name) = head.strip_prefix("s ").and_then(|r| r.split(' ').next()) else {
            continue;
        };
        let data = lines.next().unwrap_or("");
        let mut pts: Vec<(u64, f64)> = data
            .split(',')
            .filter_map(|p| {
                let (m, v) = p.split_once(':')?;
                let (m, v) = (m.parse::<u64>().ok()?, v.parse::<f64>().ok()?);
                (v.is_finite() && m + LONG_CAP as u64 > now && m <= now).then_some((m, v))
            })
            .collect();
        pts.sort_by_key(|p| p.0);
        out.push((name.to_string(), pts));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_behaviour() {
        let mut h: History = History::new(3);
        assert_eq!(h.min(), None);
        assert_eq!(h.avg(), None);
        for v in [1.0, 2.0, 3.0, 4.0] {
            h.push(v);
        }
        assert_eq!(h.len(), 3);
        assert_eq!(h.iter().collect::<Vec<_>>(), vec![2.0, 3.0, 4.0]);
        assert_eq!(h.min(), Some(2.0));
        assert_eq!(h.max(), Some(4.0));
        assert_eq!(h.avg(), Some(3.0));
        assert_eq!(h.tail(2), vec![3.0, 4.0]);
        assert_eq!(h.tail(10).len(), 3);
    }

    #[test]
    fn long_tier_averages_per_minute() {
        let mut t = Tiered::default();
        t.push_at(10.0, 100);
        t.push_at(20.0, 100);
        t.push_at(40.0, 101);
        assert_eq!(
            t.minutes().iter().copied().collect::<Vec<_>>(),
            vec![(100, 15.0)]
        );
        t.push_at(50.0, 105);
        assert_eq!(t.minutes().len(), 2);
        // 1 h view: bins for minutes 46..=105, 1 minute each; gaps repeat the last value.
        let v = t.view_at(Span::H1, 1000, 105);
        assert_eq!(v.len(), VIEW_POINTS - (100 - 46));
        assert_eq!(*v.last().unwrap(), 50.0);
        assert_eq!(v[0], 15.0);
        // 24 h view: 24-minute bins, so all three points average into one.
        let v = t.view_at(Span::H24, 1000, 105);
        assert!(v.len() > 50 && v.len() <= VIEW_POINTS);
        assert!(v.iter().all(|x| *x == 35.0));
        assert!(t.view_at(Span::H1, 1000, 100_000).is_empty());
        // Ring is capped at 24 h.
        let mut r = Tiered::default();
        for m in 0..(LONG_CAP as u64 + 50) {
            r.push_at(1.0, m);
        }
        assert_eq!(r.minutes().len(), LONG_CAP);
    }

    #[test]
    fn persistence_round_trip_and_version() {
        let dir = std::env::temp_dir().join(format!("mstop-hist-{}", std::process::id()));
        let path = dir.join("nested/history");
        let mut a = Tiered::default();
        let now = 5_000_000u64;
        for m in 0..10u64 {
            a.push_at(m as f64 + 0.5, now - 20 + m);
        }
        // Includes a point older than 24 h that must be dropped on load.
        a.push_at(99.0, now - 2000);
        a.push_at(1.0, now - 1999);
        let b = Tiered::default();
        save(&path, &[("a".into(), &a), ("b".into(), &b)]).unwrap();
        assert!(!path.with_extension("tmp").exists());
        let got = load(&path, now);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].0, "a");
        assert_eq!(got[0].1.len(), 10);
        assert_eq!(got[0].1[0], (now - 20, 0.5));
        assert!(got[1].1.is_empty());
        // Restore keeps live points ahead of loaded ones.
        let mut c = Tiered::default();
        c.restore(got[0].1.clone());
        assert_eq!(c.minutes().len(), 10);
        // Version mismatch and garbage are ignored.
        std::fs::write(&path, "mstop-history 2\ns a 1\n1:1\n").unwrap();
        assert!(load(&path, now).is_empty());
        std::fs::write(&path, "\u{0}garbage").unwrap();
        assert!(load(&path, now).is_empty());
        assert!(load(&dir.join("missing"), now).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn span_cycle_and_ten_minute_view() {
        let mut s = Span::S60;
        for _ in 0..4 {
            s = s.next();
        }
        assert_eq!(s, Span::S60);
        let mut t = Tiered::default();
        for i in 0..600 {
            t.push_at(i as f64, 1);
        }
        let v = t.view_at(Span::M10, 1000, 1);
        assert!(v.len() > 50 && v.len() <= VIEW_POINTS);
        assert!(v[0] < v[59]);
    }
}
