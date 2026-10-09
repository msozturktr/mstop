//! Application state and key handling.

use crate::alerts::Engine;
use crate::collect::{Snapshot, procs::ProcInfo};
use crate::history::{self, History, Span, Tiered};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Panel {
    Cpu,
    Mem,
    Gpu,
    Network,
    Sensors,
    Disks,
    Procs,
    Services,
}

impl Panel {
    /// All panels, in hotkey order.
    pub const ALL: [Panel; 8] = [
        Panel::Cpu,
        Panel::Mem,
        Panel::Gpu,
        Panel::Network,
        Panel::Sensors,
        Panel::Disks,
        Panel::Procs,
        Panel::Services,
    ];

    /// Focus hotkey and number shown in the panel title.
    pub fn num(self) -> u8 {
        match self {
            Panel::Cpu => 1,
            Panel::Mem => 2,
            Panel::Gpu => 3,
            Panel::Network => 4,
            Panel::Sensors => 5,
            Panel::Disks => 6,
            Panel::Procs => 7,
            Panel::Services => 8,
        }
    }

    pub fn from_key(c: char) -> Option<Panel> {
        Panel::ALL
            .into_iter()
            .find(|p| p.num().to_string() == c.to_string())
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SortKey {
    Cpu,
    Mem,
    Pid,
    Name,
    Io,
}

impl SortKey {
    pub fn label(self) -> &'static str {
        match self {
            SortKey::Cpu => "CPU",
            SortKey::Mem => "Memory",
            SortKey::Pid => "PID",
            SortKey::Name => "Name",
            SortKey::Io => "Disk I/O",
        }
    }
    fn next(self) -> Self {
        match self {
            SortKey::Cpu => SortKey::Mem,
            SortKey::Mem => SortKey::Pid,
            SortKey::Pid => SortKey::Name,
            SortKey::Name => SortKey::Io,
            SortKey::Io => SortKey::Cpu,
        }
    }
    /// Natural direction: true when largest/first-in-order should come first descending.
    fn default_desc(self) -> bool {
        matches!(self, SortKey::Cpu | SortKey::Mem | SortKey::Io)
    }
}

/// A visible process row: index into the snapshot list plus tree prefix.
#[derive(Clone, Debug)]
pub struct Row {
    pub idx: usize,
    pub prefix: String,
}

pub struct KillDialog {
    pub pid: u32,
    pub name: String,
    /// Start ticks read when the dialog opened; the signal is only sent if they still match.
    pub start_ticks: u64,
}

pub struct App {
    pub snap: Option<Snapshot>,
    pub cpu_total: Tiered,
    pub cores: Vec<History>,
    pub ram: Tiered,
    pub swap: Tiered,
    pub gpu_util: Vec<Tiered>,
    pub cpu_temp: Tiered,
    pub net_down: Tiered,
    pub net_up: Tiered,
    /// Total disk read + write bytes/s.
    pub disk_io: Tiered,
    /// Time span of every graph (`h` cycles it).
    pub span: Span,
    pub alerts: Engine,
    started: Instant,
    last_save: Instant,
    pub rows: Vec<Row>,
    pub matches: usize,
    pub sel: usize,
    sel_pid: Option<u32>,
    pub scroll: Cell<usize>,
    pub view_h: Cell<usize>,
    pub sort: SortKey,
    pub reverse: bool,
    pub filter: String,
    pub filter_mode: bool,
    pub tree: bool,
    pub focus: Panel,
    pub paused: bool,
    pub kill: Option<KillDialog>,
    pub help: bool,
    pub quit: bool,
    pub interval_ms: u64,
    pub message: Option<(String, Instant)>,
}

impl App {
    pub fn new(interval_ms: u64) -> Self {
        Self {
            snap: None,
            cpu_total: Tiered::default(),
            cores: Vec::new(),
            ram: Tiered::default(),
            swap: Tiered::default(),
            gpu_util: Vec::new(),
            cpu_temp: Tiered::default(),
            net_down: Tiered::default(),
            net_up: Tiered::default(),
            disk_io: Tiered::default(),
            span: Span::default(),
            alerts: Engine::new(),
            started: Instant::now(),
            last_save: Instant::now(),
            rows: Vec::new(),
            matches: 0,
            sel: 0,
            sel_pid: None,
            scroll: Cell::new(0),
            view_h: Cell::new(10),
            sort: SortKey::Cpu,
            reverse: false,
            filter: String::new(),
            filter_mode: false,
            tree: false,
            focus: Panel::Procs,
            paused: false,
            kill: None,
            help: false,
            quit: false,
            interval_ms,
            message: None,
        }
    }

    pub fn apply_snapshot(&mut self, snap: Snapshot) {
        if self.paused {
            return;
        }
        self.cpu_total.push(snap.cpu.total);
        if self.cores.len() != snap.cpu.cores.len() {
            self.cores = snap.cpu.cores.iter().map(|_| History::default()).collect();
        }
        for (h, c) in self.cores.iter_mut().zip(&snap.cpu.cores) {
            h.push(c.usage);
        }
        self.ram.push(snap.mem.used as f64);
        self.swap.push(snap.mem.swap_used as f64);
        if self.gpu_util.len() < snap.gpus.len() {
            self.gpu_util.resize_with(snap.gpus.len(), Tiered::default);
        }
        for (h, g) in self.gpu_util.iter_mut().zip(&snap.gpus) {
            h.push(g.util.unwrap_or(0.0));
        }
        if let Some(t) = snap.sensors.cpu_temp {
            self.cpu_temp.push(t);
        }
        self.net_down.push(snap.net.rx_bps);
        self.net_up.push(snap.net.tx_bps);
        self.disk_io
            .push(snap.disks.read_bps() + snap.disks.write_bps());
        self.alerts.evaluate(
            &snap,
            self.started.elapsed().as_secs_f64(),
            crate::collect::services::unix_now(),
        );
        self.snap = Some(snap);
        self.rebuild_rows();
    }

    /// Seconds covered by a graph at the current span.
    pub fn span_secs(&self) -> u64 {
        match self.span {
            Span::S60 => history::VIEW_POINTS as u64 * self.interval_ms / 1000,
            Span::M10 => 600,
            Span::H1 => 3600,
            Span::H24 => 86_400,
        }
    }

    /// Series persisted between runs, with their file names.
    fn long_series(&self) -> Vec<(String, &Tiered)> {
        let mut v: Vec<(String, &Tiered)> = vec![
            ("cpu_total".into(), &self.cpu_total),
            ("ram".into(), &self.ram),
            ("swap".into(), &self.swap),
            ("cpu_temp".into(), &self.cpu_temp),
            ("net_down".into(), &self.net_down),
            ("net_up".into(), &self.net_up),
            ("disk_io".into(), &self.disk_io),
        ];
        for (i, g) in self.gpu_util.iter().enumerate() {
            v.push((format!("gpu{i}"), g));
        }
        v
    }

    /// Load the persisted 24 h tier from `path`.
    pub fn load_history(&mut self, path: &std::path::Path) {
        for (name, pts) in history::load(path, history::now_minute()) {
            let t = match name.as_str() {
                "cpu_total" => &mut self.cpu_total,
                "ram" => &mut self.ram,
                "swap" => &mut self.swap,
                "cpu_temp" => &mut self.cpu_temp,
                "net_down" => &mut self.net_down,
                "net_up" => &mut self.net_up,
                "disk_io" => &mut self.disk_io,
                other => {
                    let Some(i) = other
                        .strip_prefix("gpu")
                        .and_then(|n| n.parse::<usize>().ok())
                    else {
                        continue;
                    };
                    if i >= 16 {
                        continue;
                    }
                    if self.gpu_util.len() <= i {
                        self.gpu_util.resize_with(i + 1, Tiered::default);
                    }
                    &mut self.gpu_util[i]
                }
            };
            t.restore(pts);
        }
    }

    /// Write the 24 h tier to `path` (atomic); errors are ignored, history is best effort.
    pub fn save_history(&mut self, path: &std::path::Path) {
        let _ = history::save(path, &self.long_series());
        self.last_save = Instant::now();
    }

    /// Save every five minutes.
    pub fn maybe_save_history(&mut self, path: &std::path::Path) {
        if self.last_save.elapsed() >= Duration::from_secs(300) {
            self.save_history(path);
        }
    }

    fn matches_filter(&self, p: &ProcInfo, needle: &str) -> bool {
        p.name.to_lowercase().contains(needle)
            || p.cmd.to_lowercase().contains(needle)
            || p.user.to_lowercase().contains(needle)
            || p.pid.to_string().contains(needle)
    }

    fn compare(&self, a: &ProcInfo, b: &ProcInfo) -> std::cmp::Ordering {
        let ord = match self.sort {
            SortKey::Cpu => a.cpu.total_cmp(&b.cpu),
            SortKey::Mem => a.mem.cmp(&b.mem),
            SortKey::Pid => a.pid.cmp(&b.pid),
            SortKey::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
            SortKey::Io => a.io_bps().total_cmp(&b.io_bps()),
        };
        let ord = if self.sort.default_desc() {
            ord.reverse()
        } else {
            ord
        };
        let ord = ord.then(a.pid.cmp(&b.pid));
        if self.reverse { ord.reverse() } else { ord }
    }

    pub fn sort_desc_effective(&self) -> bool {
        self.sort.default_desc() != self.reverse
    }

    pub fn rebuild_rows(&mut self) {
        let Some(snap) = &self.snap else { return };
        let list = &snap.procs.list;
        let needle = self.filter.to_lowercase();
        let matched: Vec<bool> = list
            .iter()
            .map(|p| needle.is_empty() || self.matches_filter(p, &needle))
            .collect();
        self.matches = matched.iter().filter(|m| **m).count();

        let mut rows = Vec::new();
        if !self.tree {
            let mut idx: Vec<usize> = (0..list.len()).filter(|i| matched[*i]).collect();
            idx.sort_by(|a, b| self.compare(&list[*a], &list[*b]));
            rows.extend(idx.into_iter().map(|idx| Row {
                idx,
                prefix: String::new(),
            }));
        } else {
            let pids: HashMap<u32, usize> =
                list.iter().enumerate().map(|(i, p)| (p.pid, i)).collect();
            let mut children: HashMap<u32, Vec<usize>> = HashMap::new();
            let mut roots = Vec::new();
            for (i, p) in list.iter().enumerate() {
                if p.ppid != p.pid && pids.contains_key(&p.ppid) {
                    children.entry(p.ppid).or_default().push(i);
                } else {
                    roots.push(i);
                }
            }
            // Keep a node if it or any descendant matches the filter.
            let mut keep: HashSet<usize> = HashSet::new();
            if needle.is_empty() {
                keep.extend(0..list.len());
            } else {
                for (i, m) in matched.iter().enumerate() {
                    if !*m {
                        continue;
                    }
                    let mut cur = i;
                    let mut guard = 0;
                    while keep.insert(cur) || guard == 0 {
                        guard += 1;
                        let pp = list[cur].ppid;
                        match pids.get(&pp) {
                            Some(&n) if n != cur && guard < 256 => cur = n,
                            _ => break,
                        }
                    }
                }
            }
            roots.retain(|i| keep.contains(i));
            roots.sort_by(|a, b| self.compare(&list[*a], &list[*b]));
            let mut stack: Vec<(usize, String, String, bool, bool)> = Vec::new();
            for (n, r) in roots.iter().enumerate().rev() {
                stack.push((*r, String::new(), String::new(), true, n == roots.len() - 1));
            }
            let mut seen = HashSet::new();
            while let Some((i, prefix, cont, root, _last)) = stack.pop() {
                if !seen.insert(i) {
                    continue;
                }
                rows.push(Row {
                    idx: i,
                    prefix: if root { String::new() } else { prefix },
                });
                let mut kids: Vec<usize> = children
                    .get(&list[i].pid)
                    .map(|v| v.iter().copied().filter(|k| keep.contains(k)).collect())
                    .unwrap_or_default();
                kids.sort_by(|a, b| self.compare(&list[*a], &list[*b]));
                let n = kids.len();
                for (k, c) in kids.into_iter().enumerate().rev() {
                    let last = k == n - 1;
                    let branch = if last { "└─ " } else { "├─ " };
                    let next_cont = if last { "   " } else { "│  " };
                    let base = if root { String::new() } else { cont.clone() };
                    stack.push((
                        c,
                        format!("{base}{branch}"),
                        format!("{base}{next_cont}"),
                        false,
                        last,
                    ));
                }
            }
        }
        self.rows = rows;
        // Keep the selection on the same PID when possible.
        if let Some(pid) = self.sel_pid
            && let Some(pos) = self.rows.iter().position(|r| list[r.idx].pid == pid)
        {
            self.sel = pos;
        }
        self.clamp_sel();
    }

    fn clamp_sel(&mut self) {
        if self.rows.is_empty() {
            self.sel = 0;
            self.sel_pid = None;
            return;
        }
        self.sel = self.sel.min(self.rows.len() - 1);
        self.sel_pid = self
            .snap
            .as_ref()
            .map(|s| s.procs.list[self.rows[self.sel].idx].pid);
    }

    pub fn selected(&self) -> Option<&ProcInfo> {
        let snap = self.snap.as_ref()?;
        self.rows.get(self.sel).map(|r| &snap.procs.list[r.idx])
    }

    fn move_sel(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let max = self.rows.len() as isize - 1;
        self.sel = (self.sel as isize + delta).clamp(0, max) as usize;
        self.clamp_sel();
    }

    fn send_signal(&mut self, sig: i32, label: &str) {
        let Some(d) = self.kill.take() else { return };
        // The dialog may have been open for a while: if the process exited and its pid was
        // reused, signalling it would hit an unrelated process.
        if crate::collect::procs::start_ticks(d.pid) != Some(d.start_ticks) {
            let msg = format!(
                "{} (PID {}) has already exited; nothing sent",
                d.name, d.pid
            );
            self.message = Some((msg, Instant::now()));
            return;
        }
        // SAFETY: plain kill(2) syscall with an explicit pid (> 1, checked on open) and signal.
        let rc = unsafe { libc::kill(d.pid as libc::pid_t, sig) };
        let msg = if rc == 0 {
            format!("Sent {label} to {} (PID {})", d.name, d.pid)
        } else {
            format!(
                "Could not signal {} (PID {}): {}",
                d.name,
                d.pid,
                std::io::Error::last_os_error()
            )
        };
        self.message = Some((msg, Instant::now()));
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        if key.kind == crossterm::event::KeyEventKind::Release {
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        if self.kill.is_some() {
            match key.code {
                KeyCode::Char('y') => self.send_signal(libc::SIGTERM, "SIGTERM"),
                KeyCode::Char('K') => self.send_signal(libc::SIGKILL, "SIGKILL"),
                KeyCode::Char('n') | KeyCode::Esc => self.kill = None,
                _ => {}
            }
            return;
        }
        if self.filter_mode {
            match key.code {
                KeyCode::Esc => {
                    self.filter.clear();
                    self.filter_mode = false;
                }
                KeyCode::Enter => self.filter_mode = false,
                KeyCode::Backspace => {
                    self.filter.pop();
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.filter.push(c)
                }
                _ => return,
            }
            self.rebuild_rows();
            return;
        }
        if self.help {
            if matches!(
                key.code,
                KeyCode::Esc | KeyCode::F(1) | KeyCode::Char('?') | KeyCode::Char('q')
            ) {
                self.help = false;
            }
            return;
        }
        let page = self.view_h.get().max(1) as isize;
        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::F(1) | KeyCode::Char('?') => self.help = true,
            KeyCode::Up => self.move_sel(-1),
            KeyCode::Down => self.move_sel(1),
            KeyCode::PageUp => self.move_sel(-page),
            KeyCode::PageDown => self.move_sel(page),
            KeyCode::Home => self.move_sel(isize::MIN / 2),
            KeyCode::End => self.move_sel(isize::MAX / 2),
            KeyCode::Char('/') => self.filter_mode = true,
            KeyCode::Esc => {
                if !self.filter.is_empty() {
                    self.filter.clear();
                    self.rebuild_rows();
                }
            }
            KeyCode::Char('s') => {
                self.sort = self.sort.next();
                self.reverse = false;
                self.rebuild_rows();
            }
            KeyCode::Char('S') => {
                self.reverse = !self.reverse;
                self.rebuild_rows();
            }
            KeyCode::Char('t') => {
                self.tree = !self.tree;
                self.rebuild_rows();
            }
            KeyCode::Char('k') => {
                // pid 0/1 are never offered: kill(0) signals our own process group.
                if let Some(p) = self.selected().filter(|p| p.pid > 1)
                    && let Some(start_ticks) = crate::collect::procs::start_ticks(p.pid)
                {
                    self.kill = Some(KillDialog {
                        pid: p.pid,
                        name: p.name.clone(),
                        start_ticks,
                    });
                }
            }
            KeyCode::Char('h') => self.span = self.span.next(),
            KeyCode::Char(' ') => self.paused = !self.paused,
            KeyCode::Tab => {
                let i = Panel::ALL
                    .iter()
                    .position(|p| *p == self.focus)
                    .unwrap_or(0);
                self.focus = Panel::ALL[(i + 1) % Panel::ALL.len()];
            }
            KeyCode::BackTab => {
                let i = Panel::ALL
                    .iter()
                    .position(|p| *p == self.focus)
                    .unwrap_or(0);
                self.focus = Panel::ALL[(i + Panel::ALL.len() - 1) % Panel::ALL.len()];
            }
            KeyCode::Char(c @ '1'..='9') => {
                if let Some(p) = Panel::from_key(c) {
                    self.focus = p;
                }
            }
            _ => {}
        }
    }
}
