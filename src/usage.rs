//! Per-app usage: a day of CPU and memory samples (taken once a minute while Clean You is open),
//! plus the kernel's resource counters for an app's running processes.

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const DAY: u64 = 86_400;
const EVERY: u64 = 60;
/// Apps recorded per sample: the heaviest by memory, plus anything busy on the CPU.
const PER_SAMPLE: usize = 25;

/// (unix time, CPU %, memory bytes).
pub type Sample = (u64, f32, u64);

fn file() -> PathBuf {
    crate::scan::home().join("Library/Application Support/CleanYou/usage.tsv")
}

fn now() -> u64 {
    crate::scan::now_secs()
}

pub fn parse(text: &str, since: u64) -> HashMap<String, VecDeque<Sample>> {
    let mut m: HashMap<String, VecDeque<Sample>> = HashMap::new();
    for l in text.lines() {
        let mut it = l.splitn(4, '\t');
        let (Some(t), Some(cpu), Some(mem), Some(name)) = (it.next(), it.next(), it.next(), it.next()) else { continue };
        let (Ok(t), Ok(cpu), Ok(mem)) = (t.parse::<u64>(), cpu.parse::<f32>(), mem.parse::<u64>()) else { continue };
        if t >= since {
            m.entry(name.to_string()).or_default().push_back((t, cpu, mem));
        }
    }
    m
}

#[derive(Clone, Default)]
pub struct History(Arc<Mutex<HashMap<String, VecDeque<Sample>>>>);

impl History {
    /// Loads the last day from disk (rewriting the file without older lines) and starts sampling.
    pub fn start() -> History {
        let text = fs::read_to_string(file()).unwrap_or_default();
        let since = now().saturating_sub(DAY);
        let map = parse(&text, since);
        let mut lines: Vec<(u64, String)> = Vec::new();
        for (name, v) in &map {
            lines.extend(v.iter().map(|(t, c, m)| (*t, format!("{t}\t{c:.1}\t{m}\t{name}\n"))));
        }
        lines.sort();
        if let Some(dir) = file().parent() {
            let _ = fs::create_dir_all(dir);
        }
        let _ = fs::write(file(), lines.into_iter().map(|l| l.1).collect::<String>());
        let h = History(Arc::new(Mutex::new(map)));
        let h2 = h.clone();
        std::thread::spawn(move || loop {
            h2.sample();
            std::thread::sleep(Duration::from_secs(EVERY));
        });
        h
    }

    fn sample(&self) {
        let mut procs = crate::system::top_processes(usize::MAX);
        procs.retain(|p| p.is_app || p.mem >= 100_000_000 || p.cpu >= 5.0);
        let busy: Vec<_> = procs.iter().filter(|p| p.cpu >= 5.0).map(|p| p.name.clone()).collect();
        let keep: Vec<_> = procs
            .iter()
            .enumerate()
            .filter(|(i, p)| *i < PER_SAMPLE || busy.contains(&p.name))
            .map(|(_, p)| (p.name.clone(), p.cpu, p.mem))
            .collect();
        let t = now();
        let since = t.saturating_sub(DAY);
        let mut text = String::new();
        let mut m = self.0.lock().unwrap();
        for (name, cpu, mem) in keep {
            if name.contains(['\t', '\n']) {
                continue;
            }
            text.push_str(&format!("{t}\t{cpu:.1}\t{mem}\t{name}\n"));
            let v = m.entry(name).or_default();
            v.push_back((t, cpu, mem));
            while v.front().is_some_and(|s| s.0 < since) {
                v.pop_front();
            }
        }
        drop(m);
        if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(file()) {
            let _ = f.write_all(text.as_bytes());
        }
    }

    /// Last day of samples for an app (by its name, as in the process list), oldest first.
    pub fn of(&self, name: &str) -> Vec<Sample> {
        self.0.lock().unwrap().get(name).map(|v| v.iter().copied().collect()).unwrap_or_default()
    }
}

// ---------- kernel resource counters (proc_pid_rusage) ----------

#[repr(C)]
#[derive(Default)]
struct RusageInfoV2 {
    uuid: [u8; 16],
    user_time: u64,
    system_time: u64,
    pkg_idle_wkups: u64,
    interrupt_wkups: u64,
    pageins: u64,
    wired_size: u64,
    resident_size: u64,
    phys_footprint: u64,
    proc_start_abstime: u64,
    proc_exit_abstime: u64,
    child_user_time: u64,
    child_system_time: u64,
    child_pkg_idle_wkups: u64,
    child_interrupt_wkups: u64,
    child_pageins: u64,
    child_elapsed_abstime: u64,
    diskio_bytesread: u64,
    diskio_byteswritten: u64,
}

#[repr(C)]
#[derive(Default)]
struct Timebase {
    numer: u32,
    denom: u32,
}

extern "C" {
    fn proc_pid_rusage(pid: i32, flavor: i32, buffer: *mut RusageInfoV2) -> i32;
    fn mach_timebase_info(info: *mut Timebase) -> i32;
}

/// Summed counters for a set of processes.
#[derive(Default, Clone, Debug)]
pub struct Counters {
    pub processes: usize,
    pub cpu_time: Duration,
    pub footprint: u64,
    pub disk_read: u64,
    pub disk_written: u64,
    pub idle_wakeups: u64,
    pub interrupt_wakeups: u64,
    pub pageins: u64,
}

/// Counters for `pids` (your own processes only; others are skipped by the kernel).
pub fn counters(pids: &[u32]) -> Counters {
    let mut tb = Timebase::default();
    unsafe { mach_timebase_info(&mut tb) };
    let ns = |ticks: u64| ticks as u128 * tb.numer.max(1) as u128 / tb.denom.max(1) as u128;
    let mut c = Counters::default();
    for &pid in pids {
        let mut r = RusageInfoV2::default();
        if unsafe { proc_pid_rusage(pid as i32, 2, &mut r) } != 0 {
            continue;
        }
        c.processes += 1;
        c.cpu_time += Duration::from_nanos(ns(r.user_time + r.system_time) as u64);
        c.footprint += r.phys_footprint;
        c.disk_read += r.diskio_bytesread;
        c.disk_written += r.diskio_byteswritten;
        c.idle_wakeups += r.pkg_idle_wkups;
        c.interrupt_wakeups += r.interrupt_wkups;
        c.pageins += r.pageins;
    }
    c
}

/// "3 h 12 min", "4 min 5 s", "12 s".
pub fn duration(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..=59 => format!("{s} s"),
        60..=3599 => format!("{} min {} s", s / 60, s % 60),
        _ => format!("{} h {} min", s / 3600, s % 3600 / 60),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_drops_old() {
        let text = "100\t1.5\t2000\tSafari\n200\t3.0\t4000\tSafari\nbad line\n300\t0.0\t10\tMy\tApp\n50\t9\t9\tOld\n";
        let m = parse(text, 90);
        assert_eq!(m["Safari"].len(), 2);
        assert_eq!(m["My\tApp"][0], (300, 0.0, 10));
        assert!(!m.contains_key("Old"));
    }

    #[test]
    fn counts_own_process() {
        let c = counters(&[std::process::id()]);
        assert_eq!(c.processes, 1);
        assert!(c.footprint > 0);
        assert!(c.cpu_time > Duration::ZERO);
        assert_eq!(counters(&[999_999]).processes, 0);
    }
}
