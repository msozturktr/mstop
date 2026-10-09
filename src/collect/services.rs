//! Services & containers: systemd unit/timer state via `systemctl`, container list via the
//! Docker/Podman socket. All of it runs on a background worker (never per tick).

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const REFRESH_EVERY: Duration = Duration::from_secs(10);
const SOCKET_TIMEOUT: Duration = Duration::from_secs(2);
const DOCKER_SOCK: &str = "/var/run/docker.sock";

// ---------------------------------------------------------------------------------------
// Data
// ---------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Default)]
pub struct FailedUnit {
    pub name: String,
    pub description: String,
    /// Human reason such as `exit code 1`; empty when unknown.
    pub reason: String,
    /// Local `HH:MM` of the last state change, when known.
    pub since: String,
    /// True for units of the `--user` manager.
    pub user: bool,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Systemd {
    pub running: usize,
    pub total: usize,
    pub failed: Vec<FailedUnit>,
    pub timers: usize,
    /// Next timer to fire: (unit name, unix seconds).
    pub next_timer: Option<(String, u64)>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub enum SystemdState {
    /// Worker has not reported yet.
    #[default]
    Pending,
    /// systemd is not running or `systemctl` is missing.
    Unavailable,
    Ready(Systemd),
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Container {
    pub name: String,
    pub image: String,
    pub state: String,
    pub status: String,
    pub cpu_pct: Option<f64>,
    pub mem: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub enum ContainersState {
    #[default]
    Pending,
    /// No Docker/Podman socket exists.
    NotInstalled,
    /// A socket exists but we may not open it.
    NoAccess,
    Error(String),
    Ready {
        engine: &'static str,
        list: Vec<Container>,
    },
}

#[derive(Debug, Clone, Default)]
pub struct ServicesInfo {
    pub systemd: SystemdState,
    pub containers: ContainersState,
}

pub const NO_ACCESS_HINT: &str = "add yourself to the docker group";

// ---------------------------------------------------------------------------------------
// systemctl parsers
// ---------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Unit {
    pub name: String,
    pub load: String,
    pub active: String,
    pub sub: String,
    pub description: String,
}

/// Parse `systemctl list-units --plain --no-legend` output.
pub fn parse_units(text: &str) -> Vec<Unit> {
    text.lines()
        .filter_map(|line| {
            let mut t = line.split_whitespace().peekable();
            if matches!(t.peek(), Some(&"●" | &"*" | &"○" | &"×")) {
                t.next();
            }
            let (name, load, active, sub) = (t.next()?, t.next()?, t.next()?, t.next()?);
            Some(Unit {
                name: name.to_string(),
                load: load.to_string(),
                active: active.to_string(),
                sub: sub.to_string(),
                description: t.collect::<Vec<_>>().join(" "),
            })
        })
        .collect()
}

/// (running, total loaded, failed units).
pub fn summarize_units(units: &[Unit]) -> (usize, usize, Vec<&Unit>) {
    let loaded = units.iter().filter(|u| u.load != "not-found");
    let total = loaded.clone().count();
    let running = loaded.filter(|u| u.sub == "running").count();
    let failed = units.iter().filter(|u| u.active == "failed").collect();
    (running, total, failed)
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ShowInfo {
    pub id: String,
    pub result: String,
    pub exit_status: String,
    pub changed: String,
}

/// Parse `systemctl show -p Id,Result,ExecMainStatus,StateChangeTimestamp a b c`.
pub fn parse_show(text: &str) -> Vec<ShowInfo> {
    let mut out = Vec::new();
    let mut cur = ShowInfo::default();
    let mut any = false;
    for line in text.lines().chain(std::iter::once("")) {
        if line.trim().is_empty() {
            if any {
                out.push(std::mem::take(&mut cur));
                any = false;
            }
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            any = true;
            match k {
                "Id" => cur.id = v.to_string(),
                "Result" => cur.result = v.to_string(),
                "ExecMainStatus" => cur.exit_status = v.to_string(),
                "StateChangeTimestamp" => cur.changed = v.to_string(),
                _ => {}
            }
        }
    }
    out
}

/// `exit-code` + `1` becomes `exit code 1`; unknown or successful results give "".
pub fn describe_failure(result: &str, exit_status: &str) -> String {
    match result {
        "exit-code" => format!("exit code {}", exit_status.trim()),
        "signal" => "killed by a signal".into(),
        "core-dump" => "crashed (core dumped)".into(),
        "timeout" => "timed out".into(),
        "watchdog" => "watchdog timeout".into(),
        "start-limit-hit" => "restarted too often".into(),
        "resources" => "out of resources".into(),
        "" | "success" => String::new(),
        other => other.to_string(),
    }
}

/// `Fri 2026-10-09 12:03:22 +03` becomes `12:03`.
pub fn timestamp_hm(ts: &str) -> String {
    ts.split_whitespace()
        .find(|t| t.matches(':').count() == 2)
        .map(|t| t[..5].to_string())
        .unwrap_or_default()
}

/// Undo systemd's `\xNN` escaping of unit names.
pub fn unescape_unit(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && b.get(i + 1) == Some(&b'x')
            && let Some(h) = s.get(i + 2..i + 4)
            && let Ok(v) = u8::from_str_radix(h, 16)
        {
            out.push(v);
            i += 4;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Timers from `systemctl list-timers --all --output=json`: (unit, next-fire unix seconds).
/// `None` when the output is not JSON (old systemd).
pub fn parse_timers_json(text: &str) -> Option<Vec<(String, Option<u64>)>> {
    let t = text.trim();
    if !t.starts_with('[') {
        return None;
    }
    Some(
        json_objects(t)
            .into_iter()
            .filter_map(|o| {
                let unit = json_field(o, "unit").map(json_string)?;
                let next = json_field(o, "next")
                    .and_then(|v| v.trim().parse::<u64>().ok())
                    .filter(|n| *n > 0)
                    .map(|us| us / 1_000_000);
                Some((unit, next))
            })
            .collect(),
    )
}

/// Fallback for `list-timers --plain` text: only a count of timer rows.
pub fn count_timers_text(text: &str) -> usize {
    text.lines()
        .filter(|l| l.split_whitespace().any(|w| w.ends_with(".timer")))
        .filter(|l| !l.starts_with("NEXT"))
        .count()
}

// ---------------------------------------------------------------------------------------
// Tiny JSON helpers (enough for systemctl and the Docker API)
// ---------------------------------------------------------------------------------------

/// Index just past the closing quote of the string starting at `i`.
fn string_end(b: &[u8], i: usize) -> usize {
    let mut j = i + 1;
    while j < b.len() {
        match b[j] {
            b'\\' => j += 2,
            b'"' => return j + 1,
            _ => j += 1,
        }
    }
    b.len()
}

/// Index just past the JSON value starting at `start`.
fn value_end(b: &[u8], start: usize) -> usize {
    match b.get(start) {
        None => b.len(),
        Some(b'"') => string_end(b, start),
        Some(b'{' | b'[') => {
            let mut depth = 0i32;
            let mut i = start;
            while i < b.len() {
                match b[i] {
                    b'"' => {
                        i = string_end(b, i);
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth -= 1;
                        if depth == 0 {
                            return i + 1;
                        }
                    }
                    _ => {}
                }
                i += 1;
            }
            b.len()
        }
        Some(_) => b[start..]
            .iter()
            .position(|c| matches!(c, b',' | b'}' | b']'))
            .map_or(b.len(), |p| start + p),
    }
}

fn skip_ws(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && b[i].is_ascii_whitespace() {
        i += 1;
    }
    i
}

/// Raw objects of a top-level JSON array.
pub fn json_objects(text: &str) -> Vec<&str> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut i = skip_ws(b, 0);
    if b.get(i) != Some(&b'[') {
        return out;
    }
    i += 1;
    while i < b.len() {
        i = skip_ws(b, i);
        match b.get(i) {
            Some(b'{') => {
                let e = value_end(b, i);
                out.push(&text[i..e]);
                i = e;
            }
            Some(b',') => i += 1,
            _ => break,
        }
    }
    out
}

/// Raw value of a depth-1 key of a JSON object.
pub fn json_field<'a>(obj: &'a str, key: &str) -> Option<&'a str> {
    let b = obj.as_bytes();
    let mut depth = 0i32;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'"' => {
                let end = string_end(b, i);
                if depth == 1 {
                    let j = skip_ws(b, end);
                    if b.get(j) == Some(&b':') && obj.get(i + 1..end - 1) == Some(key) {
                        let s = skip_ws(b, j + 1);
                        return Some(&obj[s..value_end(b, s)]);
                    }
                }
                i = end;
                continue;
            }
            b'{' | b'[' => depth += 1,
            b'}' | b']' => depth -= 1,
            _ => {}
        }
        i += 1;
    }
    None
}

/// Decoded string from a raw JSON string value (`null` and non-strings give "").
pub fn json_string(raw: &str) -> String {
    let raw = raw.trim();
    let Some(inner) = raw.strip_prefix('"') else {
        return String::new();
    };
    let mut out = String::new();
    let mut it = inner.chars();
    while let Some(c) = it.next() {
        match c {
            '"' => break,
            '\\' => match it.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('u') => {
                    let h: String = it.by_ref().take(4).collect();
                    out.push(
                        u32::from_str_radix(&h, 16)
                            .ok()
                            .and_then(char::from_u32)
                            .unwrap_or('?'),
                    );
                }
                Some(o) => out.push(o),
                None => break,
            },
            c => out.push(c),
        }
    }
    out
}

/// First string of a raw JSON array of strings.
fn json_first_string(raw: &str) -> String {
    let raw = raw.trim();
    match raw.strip_prefix('[') {
        Some(rest) => json_string(rest.trim_start()),
        None => json_string(raw),
    }
}

// ---------------------------------------------------------------------------------------
// Docker / Podman
// ---------------------------------------------------------------------------------------

/// Container summaries plus their ids from `GET /containers/json?all=1`.
pub fn parse_containers(body: &str) -> Vec<(String, Container)> {
    json_objects(body)
        .into_iter()
        .map(|o| {
            let get = |k: &str| json_field(o, k).map(json_string).unwrap_or_default();
            let name = json_field(o, "Names")
                .map(json_first_string)
                .unwrap_or_default();
            (
                get("Id"),
                Container {
                    name: name.trim_start_matches('/').to_string(),
                    image: get("Image"),
                    state: get("State"),
                    status: get("Status"),
                    ..Default::default()
                },
            )
        })
        .collect()
}

/// Split an HTTP response into (status code, body).
pub fn split_http(resp: &str) -> Option<(u16, &str)> {
    let (head, body) = resp.split_once("\r\n\r\n")?;
    let code = head
        .lines()
        .next()?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()?;
    Some((code, body))
}

fn socket_paths() -> Vec<(&'static str, PathBuf)> {
    let mut v = vec![("Docker", PathBuf::from(DOCKER_SOCK))];
    if let Some(rt) = std::env::var_os("XDG_RUNTIME_DIR") {
        v.push(("Podman", PathBuf::from(rt).join("podman/podman.sock")));
    }
    v
}

fn http_get(path: &PathBuf, req_path: &str) -> std::io::Result<String> {
    let mut s = UnixStream::connect(path)?;
    s.set_read_timeout(Some(SOCKET_TIMEOUT))?;
    s.set_write_timeout(Some(SOCKET_TIMEOUT))?;
    s.write_all(format!("GET {req_path} HTTP/1.0\r\nHost: localhost\r\n\r\n").as_bytes())?;
    let mut buf = Vec::new();
    s.read_to_end(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Possible cgroup v2 directories of a container id.
pub fn cgroup_candidates(id: &str, uid: u32) -> Vec<PathBuf> {
    let root = "/sys/fs/cgroup";
    vec![
        format!("{root}/system.slice/docker-{id}.scope"),
        format!("{root}/docker/{id}"),
        format!("{root}/machine.slice/libpod-{id}.scope"),
        format!(
            "{root}/user.slice/user-{uid}.slice/user@{uid}.service/user.slice/libpod-{id}.scope"
        ),
        format!(
            "{root}/user.slice/user-{uid}.slice/user@{uid}.service/app.slice/libpod-{id}.scope"
        ),
    ]
    .into_iter()
    .map(PathBuf::from)
    .collect()
}

/// `usage_usec` from a cgroup `cpu.stat`.
pub fn parse_usage_usec(cpu_stat: &str) -> Option<u64> {
    cpu_stat
        .lines()
        .find_map(|l| l.strip_prefix("usage_usec "))
        .and_then(|v| v.trim().parse().ok())
}

#[derive(Default)]
struct ContainerWorker {
    prev_cpu: HashMap<String, (u64, Instant)>,
}

impl ContainerWorker {
    fn poll(&mut self) -> ContainersState {
        let socks = socket_paths();
        let mut denied = false;
        let mut failure: Option<String> = None;
        for (engine, path) in socks.iter().filter(|(_, p)| p.exists()) {
            match http_get(path, "/containers/json?all=1") {
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => denied = true,
                Err(e) => failure = Some(format!("{engine}: {e}")),
                Ok(resp) => match split_http(&resp) {
                    Some((200, body)) => {
                        return ContainersState::Ready {
                            engine,
                            list: self.with_stats(parse_containers(body)),
                        };
                    }
                    Some((403 | 401, _)) => denied = true,
                    _ => failure = Some(format!("{engine}: unexpected reply")),
                },
            }
        }
        if let Some(f) = failure {
            ContainersState::Error(f)
        } else if denied {
            ContainersState::NoAccess
        } else {
            ContainersState::NotInstalled
        }
    }

    /// CPU and memory straight from the container's cgroup; None when it is not readable.
    fn with_stats(&mut self, list: Vec<(String, Container)>) -> Vec<Container> {
        // SAFETY: getuid(2) has no preconditions.
        let uid = unsafe { libc::getuid() };
        let now = Instant::now();
        let mut seen = HashMap::new();
        let out = list
            .into_iter()
            .map(|(id, mut c)| {
                if c.state == "running"
                    && let Some(dir) = cgroup_candidates(&id, uid)
                        .into_iter()
                        .find(|d| d.join("cpu.stat").is_file())
                {
                    c.mem = std::fs::read_to_string(dir.join("memory.current"))
                        .ok()
                        .and_then(|s| s.trim().parse().ok());
                    if let Some(us) = std::fs::read_to_string(dir.join("cpu.stat"))
                        .ok()
                        .and_then(|s| parse_usage_usec(&s))
                    {
                        if let Some((p, t)) = self.prev_cpu.get(&id) {
                            let dt = now.duration_since(*t).as_secs_f64();
                            if dt > 0.0 && us >= *p {
                                c.cpu_pct = Some((us - p) as f64 / 1e6 / dt * 100.0);
                            }
                        }
                        seen.insert(id, (us, now));
                    }
                }
                c
            })
            .collect();
        self.prev_cpu = seen;
        out
    }
}

// ---------------------------------------------------------------------------------------
// systemctl runner
// ---------------------------------------------------------------------------------------

fn systemctl(user: bool, args: &[&str]) -> Option<(bool, String)> {
    let mut cmd = Command::new("systemctl");
    if user {
        cmd.arg("--user");
    }
    let out = cmd
        .args(args)
        .arg("--no-pager")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    Some((
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    ))
}

fn collect_systemd() -> SystemdState {
    let Some((ok, text)) = systemctl(
        false,
        &[
            "list-units",
            "--type=service",
            "--all",
            "--plain",
            "--no-legend",
        ],
    ) else {
        return SystemdState::Unavailable;
    };
    let sys_units = parse_units(&text);
    if !ok && sys_units.is_empty() {
        return SystemdState::Unavailable;
    }
    let mut s = Systemd::default();
    for user in [false, true] {
        let units = if user {
            match systemctl(
                true,
                &[
                    "list-units",
                    "--type=service",
                    "--all",
                    "--plain",
                    "--no-legend",
                ],
            ) {
                Some((true, t)) => parse_units(&t),
                _ => continue,
            }
        } else {
            sys_units.clone()
        };
        let (running, total, failed) = summarize_units(&units);
        s.running += running;
        s.total += total;
        let names: Vec<&str> = failed.iter().map(|u| u.name.as_str()).collect();
        let shown = if names.is_empty() {
            Vec::new()
        } else {
            let mut args = vec![
                "show",
                "-p",
                "Id,Result,ExecMainStatus,StateChangeTimestamp",
            ];
            args.extend(names.iter().copied());
            systemctl(user, &args)
                .map(|(_, t)| parse_show(&t))
                .unwrap_or_default()
        };
        for u in failed {
            let info = shown.iter().find(|i| i.id == u.name);
            s.failed.push(FailedUnit {
                name: unescape_unit(&u.name),
                description: u.description.clone(),
                reason: info
                    .map(|i| describe_failure(&i.result, &i.exit_status))
                    .unwrap_or_default(),
                since: info.map(|i| timestamp_hm(&i.changed)).unwrap_or_default(),
                user,
            });
        }
        if let Some((_, t)) = systemctl(user, &["list-timers", "--all", "--output=json"]) {
            match parse_timers_json(&t) {
                Some(list) => {
                    s.timers += list.len();
                    for (name, next) in list {
                        if let Some(n) = next
                            && s.next_timer.as_ref().is_none_or(|(_, cur)| n < *cur)
                        {
                            s.next_timer = Some((name, n));
                        }
                    }
                }
                None => {
                    if let Some((_, t)) = systemctl(user, &["list-timers", "--all", "--plain"]) {
                        s.timers += count_timers_text(&t);
                    }
                }
            }
        }
    }
    SystemdState::Ready(s)
}

pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// `in 12 min`, `in 3 h 05 min`, `now`.
pub fn in_text(left_secs: u64) -> String {
    match left_secs {
        0..=59 => "in under a minute".into(),
        60..=3599 => format!("in {} min", left_secs / 60),
        _ => format!("in {} h {:02} min", left_secs / 3600, left_secs % 3600 / 60),
    }
}

/// `backup.timer in 12 min`, or `none`.
pub fn next_timer_text(sd: &Systemd) -> String {
    match &sd.next_timer {
        Some((name, at)) => format!("{name} {}", in_text(at.saturating_sub(unix_now()))),
        None => "none".into(),
    }
}

// ---------------------------------------------------------------------------------------
// Sampler
// ---------------------------------------------------------------------------------------

type Shared = Arc<Mutex<ServicesInfo>>;

fn worker(shared: Shared) {
    let mut containers = ContainerWorker::default();
    loop {
        let systemd = collect_systemd();
        let c = containers.poll();
        if let Ok(mut g) = shared.lock() {
            *g = ServicesInfo {
                systemd,
                containers: c,
            };
        }
        std::thread::sleep(REFRESH_EVERY);
    }
}

pub struct ServicesSampler {
    shared: Shared,
}

impl ServicesSampler {
    pub fn new() -> Self {
        let shared: Shared = Arc::default();
        let w = Arc::clone(&shared);
        let _ = std::thread::Builder::new()
            .name("mstop-services".into())
            .spawn(move || worker(w));
        Self { shared }
    }

    pub fn sample(&mut self) -> ServicesInfo {
        self.shared.lock().map(|g| g.clone()).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UNITS: &str = "\
alsa-restore.service loaded active   exited  Save/Restore Sound Card State
cups.service         loaded failed   failed  CUPS Scheduler
ananicy-cpp.service  loaded active   running Ananicy-Cpp - ANother Auto NICe daemon
auto-cpufreq.service not-found inactive dead auto-cpufreq.service
sshd.service         loaded active   running OpenBSD Secure Shell server
";

    #[test]
    fn units_parse_and_summarize() {
        let u = parse_units(UNITS);
        assert_eq!(u.len(), 5);
        assert_eq!(u[2].description, "Ananicy-Cpp - ANother Auto NICe daemon");
        let (running, total, failed) = summarize_units(&u);
        assert_eq!((running, total), (2, 4));
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].name, "cups.service");
        // A bullet in front of failed units (no --plain) is tolerated.
        let b = parse_units("● x.service loaded failed failed Thing\n\n");
        assert_eq!(b[0].name, "x.service");
    }

    #[test]
    fn show_parse_and_failure_text() {
        let t = "Id=cups.service\nResult=exit-code\nExecMainStatus=1\nStateChangeTimestamp=Fri 2026-10-09 14:28:03 +03\n\nId=b.service\nResult=success\nExecMainStatus=0\nStateChangeTimestamp=\n";
        let s = parse_show(t);
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].id, "cups.service");
        assert_eq!(
            describe_failure(&s[0].result, &s[0].exit_status),
            "exit code 1"
        );
        assert_eq!(timestamp_hm(&s[0].changed), "14:28");
        assert_eq!(timestamp_hm(&s[1].changed), "");
        assert_eq!(describe_failure("timeout", "0"), "timed out");
        assert_eq!(describe_failure("success", "0"), "");
        assert_eq!(
            unescape_unit("app-\\x2fhome\\x2ffoo.service"),
            "app-/home/foo.service"
        );
    }

    #[test]
    fn timers_json_and_fallback() {
        let j = r#"[{"next":1791539229321107,"left":1,"unit":"a.timer","activates":"a.service"},{"next":null,"unit":"b.timer","activates":"b.service"}]"#;
        let t = parse_timers_json(j).unwrap();
        assert_eq!(t.len(), 2);
        assert_eq!(t[0], ("a.timer".to_string(), Some(1791539229)));
        assert_eq!(t[1].1, None);
        assert!(parse_timers_json("NEXT LEFT").is_none());
        assert_eq!(
            count_timers_text(
                "NEXT LEFT LAST PASSED UNIT ACTIVATES\nFri x 1min a.timer a.service\n"
            ),
            1
        );
    }

    #[test]
    fn docker_json_extraction() {
        let body = r#"[{"Id":"abc123","Names":["/web","/alias"],"Image":"nginx:latest","State":"running","Status":"Up 2 hours","Labels":{"Image":"decoy","State":"x"},"Ports":[{"IP":"0.0.0.0","PrivatePort":80}]},{"Id":"def456","Names":["db"],"Image":"postgres:16","State":"exited","Status":"Exited (0) 3 days ago"}]"#;
        let c = parse_containers(body);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].0, "abc123");
        assert_eq!(c[0].1.name, "web");
        assert_eq!(c[0].1.image, "nginx:latest");
        assert_eq!(c[0].1.state, "running");
        assert_eq!(c[0].1.status, "Up 2 hours");
        assert_eq!(c[1].1.name, "db");
        assert_eq!(c[1].1.state, "exited");
        assert!(parse_containers("[]").is_empty());
        assert!(parse_containers("not json").is_empty());
        let (code, b) = split_http("HTTP/1.0 200 OK\r\nA: b\r\n\r\n[]").unwrap();
        assert_eq!((code, b), (200, "[]"));
    }

    #[test]
    fn cgroup_helpers() {
        assert_eq!(
            parse_usage_usec("usage_usec 1234\nuser_usec 1\n"),
            Some(1234)
        );
        assert_eq!(parse_usage_usec("nope"), None);
        let c = cgroup_candidates("id1", 1000);
        assert!(c[0].ends_with("system.slice/docker-id1.scope"));
        assert!(c.iter().any(|p| p.to_string_lossy().contains("user@1000")));
    }
}
