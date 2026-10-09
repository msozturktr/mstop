//! Process sampler built on sysinfo; I/O rates come from per-refresh deltas.

use std::collections::HashMap;
use sysinfo::{Process, ProcessStatus, System, Users};

#[derive(Debug, Clone, Default)]
pub struct ProcInfo {
    pub pid: u32,
    pub ppid: u32,
    pub user: String,
    pub name: String,
    pub cmd: String,
    /// 100% = one full core (like top).
    pub cpu: f64,
    pub mem: u64,
    pub read_bps: f64,
    pub write_bps: f64,
    pub state: char,
    pub threads: u32,
    /// GPU engine usage (0-100) and VRAM; None when the process does not use a GPU.
    pub gpu_pct: Option<f64>,
    pub gpu_vram: Option<u64>,
    /// Seconds since the Unix epoch.
    pub start_time: u64,
}

impl ProcInfo {
    pub fn io_bps(&self) -> f64 {
        self.read_bps + self.write_bps
    }
}

#[derive(Debug, Clone, Default)]
pub struct ProcDetail {
    pub open_files: Option<usize>,
    pub vms: Option<u64>,
    pub nice: Option<i32>,
    pub cgroup: Option<String>,
}

/// `nice` out of /proc/PID/stat (field 19; fields are counted after the `)` closing comm).
pub fn parse_nice(stat: &str) -> Option<i32> {
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(16)?.parse().ok()
}

/// `starttime` in clock ticks since boot out of /proc/PID/stat (field 22). Together with the
/// pid it identifies one process instance, so a reused pid can be told apart.
pub fn parse_start_ticks(stat: &str) -> Option<u64> {
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(19)?.parse().ok()
}

/// Fresh start ticks for a live pid; `None` if it has exited.
pub fn start_ticks(pid: u32) -> Option<u64> {
    parse_start_ticks(&std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
}

/// Last component of the unified-hierarchy cgroup path in /proc/PID/cgroup.
pub fn parse_cgroup(text: &str) -> Option<String> {
    let line = text
        .lines()
        .find(|l| l.starts_with("0::"))
        .or_else(|| text.lines().next())?;
    let path = line.rsplit(':').next()?.trim_end_matches('/');
    let last = path.rsplit('/').next().unwrap_or("");
    Some(if last.is_empty() {
        "/".into()
    } else {
        last.into()
    })
}

/// Extra facts for one process; only called for the selected PID (touches /proc/PID/fd).
pub fn detail(pid: u32) -> ProcDetail {
    let rd = |f: &str| std::fs::read_to_string(format!("/proc/{pid}/{f}")).ok();
    ProcDetail {
        open_files: std::fs::read_dir(format!("/proc/{pid}/fd"))
            .ok()
            .map(|d| d.count()),
        vms: rd("statm")
            .and_then(|t| t.split_whitespace().next()?.parse::<u64>().ok())
            // SAFETY: sysconf has no preconditions.
            .map(|pages| pages * unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(1) as u64),
        nice: rd("stat").and_then(|t| parse_nice(&t)),
        cgroup: rd("cgroup").and_then(|t| parse_cgroup(&t)),
    }
}

#[derive(Debug, Clone, Default)]
pub struct ProcsInfo {
    pub list: Vec<ProcInfo>,
    pub total: usize,
    pub running: usize,
    pub sleeping: usize,
    pub threads: usize,
}

pub struct ProcSampler {
    users: Users,
    names: HashMap<u32, String>,
}

/// Thread count from the link count of /proc/PID/task (= threads + 2); one cheap stat call.
fn thread_count(pid: u32) -> u32 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(format!("/proc/{pid}/task"))
        .map(|m| (m.nlink() as u32).saturating_sub(2).max(1))
        .unwrap_or(1)
}

fn state_char(s: ProcessStatus) -> char {
    match s {
        ProcessStatus::Run => 'R',
        ProcessStatus::Sleep | ProcessStatus::Idle => 'S',
        ProcessStatus::UninterruptibleDiskSleep => 'D',
        ProcessStatus::Zombie => 'Z',
        ProcessStatus::Stop | ProcessStatus::Tracing => 'T',
        _ => '?',
    }
}

impl ProcSampler {
    pub fn new() -> Self {
        Self {
            users: Users::new_with_refreshed_list(),
            names: HashMap::new(),
        }
    }

    fn user_name(&mut self, p: &Process) -> String {
        let Some(uid) = p.user_id() else {
            return "?".into();
        };
        let key = **uid;
        if let Some(n) = self.names.get(&key) {
            return n.clone();
        }
        let n = self
            .users
            .get_user_by_id(uid)
            .map(|u| u.name().to_string())
            .unwrap_or_else(|| key.to_string());
        self.names.insert(key, n.clone());
        n
    }

    /// `elapsed` is the number of seconds since the previous refresh.
    pub fn sample(&mut self, sys: &System, elapsed: f64) -> ProcsInfo {
        let elapsed = elapsed.max(0.05);
        let mut out = ProcsInfo::default();
        for p in sys.processes().values() {
            // Skip individual threads that sysinfo may list as separate entries.
            if p.thread_kind().is_some() {
                continue;
            }
            let state = state_char(p.status());
            let threads = thread_count(p.pid().as_u32());
            let name = p.name().to_string_lossy().into_owned();
            let mut cmd = p
                .cmd()
                .iter()
                .map(|s| s.to_string_lossy())
                .collect::<Vec<_>>()
                .join(" ");
            if cmd.is_empty() {
                cmd = name.clone();
            }
            let du = p.disk_usage();
            out.list.push(ProcInfo {
                pid: p.pid().as_u32(),
                ppid: p.parent().map(|p| p.as_u32()).unwrap_or(0),
                user: self.user_name(p),
                name,
                cmd,
                cpu: p.cpu_usage() as f64,
                mem: p.memory(),
                read_bps: du.read_bytes as f64 / elapsed,
                write_bps: du.written_bytes as f64 / elapsed,
                state,
                threads,
                gpu_pct: None,
                gpu_vram: None,
                start_time: p.start_time(),
            });
            out.threads += threads as usize;
            match state {
                'R' => out.running += 1,
                _ => out.sleeping += 1,
            }
        }
        out.total = out.list.len();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nice_and_cgroup() {
        let stat = "123 (my (weird) name) S 1 123 123 0 -1 4194560 100 0 0 0 5 3 0 0 20 -5 4 0 100 1000 50";
        assert_eq!(parse_start_ticks(stat), Some(100));
        assert_eq!(parse_nice(stat), Some(-5));
        assert_eq!(
            parse_cgroup("0::/user.slice/user-1000.slice/app.slice/foo.scope\n").as_deref(),
            Some("foo.scope")
        );
        assert_eq!(parse_cgroup("0::/\n").as_deref(), Some("/"));
    }
}
