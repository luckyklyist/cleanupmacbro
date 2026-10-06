use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn run(cmd: &str, args: &[&str]) -> String {
    Command::new(cmd)
        .args(args)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

/// (free, total) bytes on the data volume.
pub fn disk() -> Option<(u64, u64)> {
    let out = run("/bin/df", &["-k", "/System/Volumes/Data"]);
    let f: Vec<u64> = out.lines().nth(1)?.split_whitespace().skip(1).take(3).filter_map(|x| x.parse().ok()).collect();
    (f.len() == 3).then(|| (f[2] * 1024, f[0] * 1024))
}

/// (used, total) bytes of RAM, counted like Activity Monitor's "Memory Used".
pub fn memory() -> Option<(u64, u64)> {
    let total: u64 = run("/usr/sbin/sysctl", &["-n", "hw.memsize"]).trim().parse().ok()?;
    let vm = run("/usr/bin/vm_stat", &[]);
    let page: u64 = vm.split("page size of ").nth(1)?.split_whitespace().next()?.parse().ok()?;
    let pages = |key: &str| -> u64 {
        vm.lines()
            .find(|l| l.starts_with(key))
            .and_then(|l| l.rsplit(':').next())
            .and_then(|v| v.trim().trim_end_matches('.').parse().ok())
            .unwrap_or(0)
    };
    let used = (pages("Pages active") + pages("Pages wired down") + pages("Pages occupied by compressor")) * page;
    Some((used.min(total), total))
}

#[derive(Clone)]
pub struct Proc {
    pub pids: Vec<u32>,
    /// The .app bundle, for its icon.
    pub app_path: Option<std::path::PathBuf>,
    /// App name (all helper processes of an .app are grouped together).
    pub name: String,
    pub is_app: bool,
    pub mem: u64,
    pub cpu: f32,
}

/// Your own apps and processes, heaviest memory users first. Helpers are folded into their app.
pub fn top_processes(n: usize) -> Vec<Proc> {
    let mut procs = apps();
    procs.sort_by(|a, b| b.mem.cmp(&a.mem));
    procs.truncate(n);
    procs
}

pub fn top_by_cpu(n: usize) -> Vec<Proc> {
    let mut procs = apps();
    procs.sort_by(|a, b| b.cpu.total_cmp(&a.cpu));
    procs.truncate(n);
    procs
}

fn apps() -> Vec<Proc> {
    let user = std::env::var("USER").unwrap_or_default();
    let out = run("/bin/ps", &["-U", &user, "-o", "pid=,rss=,pcpu=,comm="]);
    let mut procs: Vec<Proc> = Vec::new();
    for l in out.lines() {
        let mut it = l.split_whitespace();
        let (Some(pid), Some(rss), Some(cpu)) = (it.next(), it.next(), it.next()) else { continue };
        let (Ok(pid), Ok(rss), Ok(cpu)) = (pid.parse::<u32>(), rss.parse::<u64>(), cpu.parse::<f32>()) else { continue };
        let comm = it.collect::<Vec<_>>().join(" ");
        // "/Applications/Google Chrome.app/Contents/.../Helper" -> "Google Chrome"
        let app = comm.split('/').find(|c| c.ends_with(".app")).map(|c| c.trim_end_matches(".app").to_string());
        let app_path = crate::thumbs::bundle_of(&comm);
        let is_app = app.is_some();
        let name = app.unwrap_or_else(|| comm.rsplit('/').next().unwrap_or(&comm).to_string());
        if name == "clean-you" || name == "Clean You" {
            continue;
        }
        match procs.iter_mut().find(|p| p.name == name) {
            Some(p) => {
                p.pids.push(pid);
                p.mem += rss * 1024;
                p.cpu += cpu;
            }
            None => procs.push(Proc { pids: vec![pid], app_path, name, is_app, mem: rss * 1024, cpu }),
        }
    }
    procs
}

pub fn quit(p: &Proc) {
    if p.is_app {
        let script = format!("tell application \"{}\" to quit", esc(&p.name));
        let _ = Command::new("/usr/bin/osascript").args(["-e", &script]).spawn();
    } else {
        for pid in &p.pids {
            let _ = Command::new("/bin/kill").arg(pid.to_string()).status();
        }
    }
}

fn esc(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Shows a macOS notification banner.
pub fn notify(title: &str, body: &str) {
    let script = format!(
        "display notification \"{}\" with title \"Clean You\" subtitle \"{}\" sound name \"Funk\"",
        esc(body),
        esc(title)
    );
    let _ = Command::new("/usr/bin/osascript").args(["-e", &script]).status();
}

/// Runs a shell command with the macOS admin password prompt.
pub fn admin(cmd: &str) -> Result<(), String> {
    let script = format!("do shell script \"{cmd}\" with administrator privileges");
    let out = Command::new("/usr/bin/osascript").args(["-e", &script]).output().map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

pub fn open(target: &str) {
    let _ = Command::new("/usr/bin/open").arg(target).spawn();
}

pub fn reveal(path: &std::path::Path) {
    let _ = Command::new("/usr/bin/open").arg("-R").arg(path).spawn();
}

/// Opens a file or folder with its default app.
pub fn open_path(path: &std::path::Path) {
    let _ = Command::new("/usr/bin/open").arg(path).spawn();
}

static QUICK_LOOK: Mutex<Option<std::process::Child>> = Mutex::new(None);

/// Shows the macOS Quick Look panel for a file (replacing any open one).
pub fn quick_look(path: &std::path::Path) {
    let mut slot = QUICK_LOOK.lock().unwrap();
    if let Some(mut c) = slot.take() {
        let _ = c.kill();
        let _ = c.wait();
    }
    *slot = Command::new("/usr/bin/qlmanage")
        .arg("-p")
        .arg(path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok();
}

// ---------- live CPU / memory / app sampling ----------

extern "C" {
    fn mach_host_self() -> u32;
    fn host_statistics(host: u32, flavor: i32, info: *mut u32, count: *mut u32) -> i32;
}

/// Total (busy, all) CPU ticks since boot, across all cores.
fn cpu_ticks() -> Option<(u64, u64)> {
    const HOST_CPU_LOAD_INFO: i32 = 3;
    let mut t = [0u32; 4]; // user, system, idle, nice
    let mut count = 4u32;
    let kr = unsafe { host_statistics(mach_host_self(), HOST_CPU_LOAD_INFO, t.as_mut_ptr(), &mut count) };
    (kr == 0).then(|| {
        let busy = t[0] as u64 + t[1] as u64 + t[3] as u64;
        (busy, busy + t[2] as u64)
    })
}

pub const HISTORY: usize = 60;

#[derive(Default, Clone)]
pub struct Live {
    /// Last minute of samples, 0..=1, oldest first.
    pub cpu: Vec<f32>,
    pub mem_hist: Vec<f32>,
    pub mem: Option<(u64, u64)>,
    pub procs: Vec<Proc>,
    pub cores: usize,
}

/// Samples CPU, memory and apps once a second while someone has called `want` recently.
pub struct Sampler {
    pub live: Arc<Mutex<Live>>,
    wanted: Arc<Mutex<Instant>>,
}

impl Sampler {
    pub fn start(ctx: eframe::egui::Context) -> Sampler {
        let live = Arc::new(Mutex::new(Live {
            cores: std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1),
            mem: memory(),
            ..Default::default()
        }));
        let wanted = Arc::new(Mutex::new(Instant::now()));
        let (l, w) = (live.clone(), wanted.clone());
        std::thread::spawn(move || {
            let mut prev = cpu_ticks();
            loop {
                std::thread::sleep(Duration::from_millis(1000));
                let now = cpu_ticks();
                if w.lock().unwrap().elapsed() > Duration::from_secs(3) {
                    prev = now;
                    continue;
                }
                let cpu = match (prev, now) {
                    (Some((b0, a0)), Some((b1, a1))) if a1 > a0 => (b1 - b0) as f32 / (a1 - a0) as f32,
                    _ => 0.0,
                };
                prev = now;
                let mem = memory();
                let procs = apps();
                let mut g = l.lock().unwrap();
                push(&mut g.cpu, cpu.clamp(0.0, 1.0));
                if let Some((u, t)) = mem {
                    push(&mut g.mem_hist, u as f32 / t.max(1) as f32);
                }
                g.mem = mem.or(g.mem);
                g.procs = procs;
                drop(g);
                ctx.request_repaint();
            }
        });
        Sampler { live, wanted }
    }

    /// Keeps sampling going for a few more seconds (call every frame a page shows live data).
    pub fn want(&self) {
        *self.wanted.lock().unwrap() = Instant::now();
    }
}

fn push(v: &mut Vec<f32>, x: f32) {
    v.push(x);
    if v.len() > HISTORY {
        v.remove(0);
    }
}

// ---------- open network ports ----------

#[derive(Clone, Debug, PartialEq)]
pub struct Port {
    pub pid: u32,
    pub command: String,
    pub proto: &'static str,
    pub port: u16,
    /// Address it listens on: "*" (every network), "127.0.0.1", "[::1]", …
    pub addr: String,
}

impl Port {
    /// Reachable from other devices on the network, not just this Mac.
    pub fn exposed(&self) -> bool {
        !(self.addr.starts_with("127.") || self.addr == "[::1]" || self.addr == "localhost")
    }
}

/// Parses `lsof -F pcPn` output. Keeps listening sockets only (UDP ones with no remote end).
pub fn parse_ports(text: &str) -> Vec<Port> {
    let (mut pid, mut command, mut proto) = (0u32, String::new(), "");
    let mut out: Vec<Port> = Vec::new();
    for l in text.lines() {
        let (tag, v) = l.split_at(l.len().min(1));
        match tag {
            "p" => pid = v.parse().unwrap_or(0),
            "c" => command = v.to_string(),
            "P" => proto = if v == "UDP" { "UDP" } else { "TCP" },
            "n" if !v.contains("->") => {
                let Some((addr, port)) = v.rsplit_once(':') else { continue };
                let Ok(port) = port.parse::<u16>() else { continue };
                let p = Port { pid, command: command.clone(), proto, port, addr: addr.to_string() };
                if !out.iter().any(|o| o.pid == p.pid && o.port == p.port && o.proto == p.proto) {
                    out.push(p);
                }
            }
            _ => {}
        }
    }
    out
}

/// Ports your apps are listening on, lowest port first.
pub fn open_ports() -> Vec<Port> {
    let mut v = parse_ports(&run("/usr/sbin/lsof", &["-nP", "-iTCP", "-sTCP:LISTEN", "-FpcPn"]));
    v.extend(parse_ports(&run("/usr/sbin/lsof", &["-nP", "-iUDP", "-FpcPn"])));
    v.sort_by(|a, b| a.port.cmp(&b.port).then(a.proto.cmp(b.proto)));
    v
}

/// Asks a process to quit (SIGTERM).
pub fn stop_pid(pid: u32) {
    let _ = Command::new("/bin/kill").arg(pid.to_string()).status();
}

// ---------- keep awake (wraps macOS `caffeinate`) ----------

fn awake_file() -> std::path::PathBuf {
    crate::scan::home().join("Library/Application Support/CleanYou/awake")
}

/// Keeps the Mac (and display) awake for `secs`, or until stopped when `None`.
pub fn keep_awake(secs: Option<u64>) {
    stop_awake();
    let mut cmd = Command::new("/usr/bin/caffeinate");
    cmd.arg("-dims");
    if let Some(s) = secs {
        cmd.args(["-t", &s.to_string()]);
    }
    if let Ok(child) = cmd.spawn() {
        let until = secs.map(|s| now() + s).unwrap_or(0);
        let _ = std::fs::create_dir_all(awake_file().parent().unwrap());
        let _ = std::fs::write(awake_file(), format!("{} {until}", child.id()));
    }
}

pub fn stop_awake() {
    if let Some((pid, _)) = awake_info() {
        let _ = Command::new("/bin/kill").arg(pid.to_string()).status();
    }
    let _ = std::fs::remove_file(awake_file());
}

fn awake_info() -> Option<(u32, u64)> {
    let s = std::fs::read_to_string(awake_file()).ok()?;
    let mut it = s.split_whitespace();
    Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
}

/// `None` = not keeping awake, `Some(0)` = indefinitely, `Some(t)` = until unix time t.
pub fn awake_until() -> Option<u64> {
    let (pid, until) = awake_info()?;
    let alive = Command::new("/bin/kill").args(["-0", &pid.to_string()]).status().map_or(false, |s| s.success());
    (alive && (until == 0 || until > now())).then_some(until)
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lsof_ports() {
        let text = "p444\ncrapportd\nf8\nPTCP\nn*:61520\nf9\nPTCP\nn*:61520\np518\ncRaycast\nf30\nPTCP\nn127.0.0.1:7265\n\
                    p9\ncnode\nf3\nPTCP\nn[::1]:3000\np485\ncidentityservicesd\nf20\nPUDP\nn*:*\nf21\nPUDP\nn10.0.0.2:5353->1.2.3.4:53\nf22\nPUDP\nn*:5353\n";
        let v = parse_ports(text);
        assert_eq!(v.len(), 4, "{v:?}");
        assert_eq!((v[0].command.as_str(), v[0].port, v[0].exposed()), ("rapportd", 61520, true));
        assert!(!v[1].exposed() && !v[2].exposed());
        assert_eq!((v[3].proto, v[3].port), ("UDP", 5353));
    }
}
