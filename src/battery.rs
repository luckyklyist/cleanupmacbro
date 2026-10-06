//! Battery health info, the 80% charge limiter (root helper) and its installer.

use crate::smc;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Clone, Default)]
pub struct Battery {
    pub pct: u32,
    pub charging: bool,
    pub external: bool,
    pub cycles: u32,
    /// Real capacity vs. design capacity, 0..1.
    pub health: f32,
    pub max_mah: u32,
    pub design_mah: u32,
    pub temp_c: f32,
    pub minutes: Option<u32>,
    pub watts: Option<u32>,
}

pub fn info() -> Option<Battery> {
    let out = Command::new("/usr/sbin/ioreg").args(["-r", "-n", "AppleSmartBattery", "-a"]).output().ok()?;
    let v = plist::Value::from_reader(std::io::Cursor::new(out.stdout)).ok()?;
    let d = v.as_array()?.first()?.as_dictionary()?;
    let int = |k: &str| d.get(k).and_then(|x| x.as_signed_integer()).unwrap_or(0);
    let flag = |k: &str| d.get(k).and_then(|x| x.as_boolean()).unwrap_or(false);
    let max = int("AppleRawMaxCapacity").max(0) as u32;
    let design = int("DesignCapacity").max(1) as u32;
    let mins = int("TimeRemaining");
    let watts = d
        .get("AdapterDetails")
        .and_then(|a| a.as_dictionary()?.get("Watts")?.as_signed_integer())
        .map(|w| w as u32);
    Some(Battery {
        pct: int("CurrentCapacity").clamp(0, 100) as u32,
        charging: flag("IsCharging"),
        external: flag("ExternalConnected"),
        cycles: int("CycleCount") as u32,
        health: (max as f32 / design as f32).min(1.0),
        max_mah: max,
        design_mah: design,
        temp_c: int("Temperature") as f32 / 100.0,
        minutes: (mins > 0 && mins < 6000).then_some(mins as u32),
        watts,
    })
}

// ---------- shared config between the app and the root helper ----------

pub fn shared_dir() -> PathBuf {
    PathBuf::from("/Users/Shared/CleanYou")
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn read_kv(path: &Path) -> Vec<(String, String)> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split_once('=').map(|(k, v)| (k.trim().to_string(), v.trim().to_string())))
        .collect()
}

fn get<T: std::str::FromStr>(kv: &[(String, String)], key: &str, default: T) -> T {
    kv.iter().find(|(k, _)| k == key).and_then(|(_, v)| v.parse().ok()).unwrap_or(default)
}

#[derive(Clone, PartialEq)]
pub struct ChargeConf {
    pub enabled: bool,
    pub limit: u32,
    /// Sailing: let the battery drift this many % below the limit before charging again.
    pub sail: u32,
    /// Pause charging while the battery is hotter than 35 °C.
    pub heat_guard: bool,
    /// Unix time until which charging to 100% is allowed ("top up once").
    pub topup_until: u64,
    /// When plugged in above the limit, run from the battery down to the limit.
    pub discharge: bool,
    /// MagSafe light: orange while charging, green when holding at the limit.
    pub led: bool,
    /// Unix time a calibration was requested (0 = none).
    pub calibrate_since: u64,
    /// Scheduled full charge: weekdays bitmask (bit 0 = Monday) and the hour it should be full by.
    pub sched_days: u8,
    pub sched_hour: u32,
}

impl Default for ChargeConf {
    fn default() -> Self {
        ChargeConf {
            enabled: false,
            limit: 80,
            sail: 5,
            heat_guard: true,
            topup_until: 0,
            discharge: false,
            led: false,
            calibrate_since: 0,
            sched_days: 0,
            sched_hour: 8,
        }
    }
}

impl ChargeConf {
    pub fn load() -> Self {
        let kv = read_kv(&shared_dir().join("charge.conf"));
        let d = ChargeConf::default();
        ChargeConf {
            enabled: get(&kv, "enabled", d.enabled),
            limit: get(&kv, "limit", d.limit).clamp(50, 100),
            sail: get(&kv, "sail", d.sail).clamp(1, 30),
            heat_guard: get(&kv, "heat_guard", d.heat_guard),
            topup_until: get(&kv, "topup_until", d.topup_until),
            discharge: get(&kv, "discharge", d.discharge),
            led: get(&kv, "led", d.led),
            calibrate_since: get(&kv, "calibrate_since", d.calibrate_since),
            sched_days: get(&kv, "sched_days", d.sched_days),
            sched_hour: get(&kv, "sched_hour", d.sched_hour).min(23),
        }
    }

    pub fn save(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(shared_dir())?;
        let s = format!(
            "enabled={}\nlimit={}\nsail={}\nheat_guard={}\ntopup_until={}\ndischarge={}\nled={}\ncalibrate_since={}\nsched_days={}\nsched_hour={}\n",
            self.enabled, self.limit, self.sail, self.heat_guard, self.topup_until, self.discharge, self.led,
            self.calibrate_since, self.sched_days, self.sched_hour
        );
        std::fs::write(shared_dir().join("charge.conf"), s)
    }

    pub fn topping_up(&self) -> bool {
        now() < self.topup_until
    }

    pub fn start_topup(&mut self) {
        self.topup_until = now() + 8 * 3600;
    }

    pub fn start_calibration(&mut self) {
        self.calibrate_since = now();
    }
}

pub struct HelperStatus {
    pub alive: bool,
    pub charging_allowed: bool,
    pub adapter: bool,
    pub reason: String,
    /// 0 = not calibrating, 1..=3 = step, 4 = finished.
    pub calib_stage: u8,
}

pub fn helper_status() -> HelperStatus {
    let kv = read_kv(&shared_dir().join("charge.status"));
    let updated: u64 = get(&kv, "updated", 0);
    HelperStatus {
        alive: helper_installed() && now().saturating_sub(updated) < 120,
        charging_allowed: get(&kv, "charging", true),
        adapter: get(&kv, "adapter", true),
        reason: get(&kv, "reason", String::new()),
        calib_stage: get(&kv, "calib", 0),
    }
}

// ---------- root helper (runs as a LaunchDaemon) ----------

/// What the helper should do right now.
#[derive(Debug, PartialEq)]
pub struct Plan {
    pub charge: bool,
    /// false = run from the battery even though the charger is plugged in.
    pub adapter: bool,
    pub reason: String,
}

/// Calibration progress kept by the helper: which request it belongs to and the current step.
#[derive(Clone, Copy, Default, PartialEq, Debug)]
pub struct Calib {
    pub since: u64,
    /// 1 = charge to 100, 2 = discharge to 15, 3 = charge to 100, 4 = done.
    pub stage: u8,
}

fn plan_for(text: &str, charge: bool, adapter: bool) -> Plan {
    Plan { charge, adapter, reason: text.to_string() }
}

/// Pure decision logic. `charging` is the current SMC state (gives hysteresis), `now_local` is
/// (weekday 0 = Monday, hour) for the schedule.
pub fn plan(cfg: &ChargeConf, b: &Battery, charging: bool, calib: &mut Calib, now_local: (u32, u32)) -> Plan {
    // Calibration overrides everything: 100% → 15% → 100%.
    if cfg.calibrate_since == 0 {
        *calib = Calib::default();
    } else if calib.since != cfg.calibrate_since {
        *calib = Calib { since: cfg.calibrate_since, stage: 1 };
    }
    match calib.stage {
        1 if b.pct >= 100 => calib.stage = 2,
        2 if b.pct <= 15 => calib.stage = 3,
        3 if b.pct >= 100 => calib.stage = 4,
        _ => {}
    }
    match calib.stage {
        1 => return plan_for("Calibrating 1/3: charging to 100%", true, true),
        2 => return plan_for("Calibrating 2/3: running down to 15%", false, false),
        3 => return plan_for("Calibrating 3/3: charging back to 100%", true, true),
        _ => {}
    }

    if !cfg.enabled {
        return plan_for("Limit off", true, true);
    }
    if cfg.topping_up() {
        return plan_for("Topping up to 100%", b.pct < 100, true);
    }
    let (day, hour) = now_local;
    let start = cfg.sched_hour.saturating_sub(3);
    if cfg.sched_days & (1 << day) != 0 && hour >= start && hour < cfg.sched_hour {
        return Plan {
            charge: b.pct < 100,
            adapter: true,
            reason: format!("Scheduled: full by {}:00", cfg.sched_hour),
        };
    }
    if cfg.heat_guard && b.temp_c >= 35.0 {
        return plan_for("Paused: battery is warm", false, true);
    }
    if cfg.discharge && b.external && b.pct > cfg.limit {
        return Plan { charge: false, adapter: false, reason: format!("Discharging to {}%", cfg.limit) };
    }
    let floor = cfg.limit.saturating_sub(cfg.sail);
    if b.pct >= cfg.limit {
        plan_for("Holding at limit", false, true)
    } else if b.pct <= floor {
        plan_for("Charging to limit", true, true)
    } else if charging {
        plan_for("Charging to limit", true, true)
    } else {
        Plan { charge: false, adapter: true, reason: format!("Sailing ({}–{}%)", floor, cfg.limit) }
    }
}

/// (weekday 0 = Monday, hour) in local time.
fn local_now() -> (u32, u32) {
    let out = Command::new("/bin/date").arg("+%u %H").output().map(|o| String::from_utf8_lossy(&o.stdout).to_string()).unwrap_or_default();
    let mut it = out.split_whitespace();
    let day = it.next().and_then(|d| d.parse::<u32>().ok()).unwrap_or(1).saturating_sub(1);
    let hour = it.next().and_then(|h| h.parse().ok()).unwrap_or(12);
    (day, hour)
}

pub fn run_daemon() -> ! {
    let Some(smc) = smc::Smc::open() else {
        eprintln!("Can't open SMC");
        std::process::exit(1);
    };
    // Never start with the adapter cut off (e.g. after a crash mid-discharge).
    let _ = smc::set_adapter(&smc, true);
    let state_file = shared_dir().join("calib.state");
    let kv = read_kv(&state_file);
    let mut calib = Calib { since: get(&kv, "since", 0), stage: get(&kv, "stage", 0) };
    let mut led_on = false;
    loop {
        let cfg = ChargeConf::load();
        let mut reason = String::from("No battery found");
        let mut charging = smc::charging_allowed(&smc).unwrap_or(true);
        let mut adapter = smc::adapter_enabled(&smc);
        if let Some(b) = info() {
            let before = calib;
            let p = plan(&cfg, &b, charging, &mut calib, local_now());
            reason = p.reason;
            if calib != before {
                let _ = std::fs::write(&state_file, format!("since={}\nstage={}\n", calib.since, calib.stage));
            }
            if p.charge != charging {
                match smc::set_charging(&smc, p.charge) {
                    Ok(()) => charging = p.charge,
                    Err(e) => reason = e,
                }
            }
            if p.adapter != adapter {
                match smc::set_adapter(&smc, p.adapter) {
                    Ok(()) => adapter = p.adapter,
                    Err(e) => reason = e,
                }
            }
            if cfg.led && b.external {
                let led = if b.charging { smc::Led::Orange } else { smc::Led::Green };
                let _ = smc::set_led(&smc, led);
                led_on = true;
            } else if led_on {
                let _ = smc::set_led(&smc, smc::Led::System);
                led_on = false;
            }
        }
        let _ = std::fs::create_dir_all(shared_dir());
        let _ = std::fs::write(
            shared_dir().join("charge.status"),
            format!("updated={}\ncharging={charging}\nadapter={adapter}\ncalib={}\nreason={reason}\n", now(), calib.stage),
        );
        let busy = cfg.enabled || calib.stage > 0 && calib.stage < 4;
        std::thread::sleep(Duration::from_secs(if busy { 20 } else { 60 }));
    }
}

/// Restores normal charging, power adapter and MagSafe light (used when uninstalling).
pub fn reset_charging() {
    if let Some(smc) = smc::Smc::open() {
        let _ = smc::set_charging(&smc, true);
        let _ = smc::set_adapter(&smc, true);
        let _ = smc::set_led(&smc, smc::Led::System);
    }
}

// ---------- install / uninstall (asks for the admin password) ----------

const LABEL: &str = "local.cleanyou.charge";
const HELPER: &str = "/Library/Application Support/CleanYou/clean-you-helper";

fn daemon_plist_path() -> String {
    format!("/Library/LaunchDaemons/{LABEL}.plist")
}

pub fn helper_installed() -> bool {
    Path::new(&daemon_plist_path()).exists()
}

pub fn supported() -> bool {
    smc::Smc::open().map_or(false, |s| smc::charge_key(&s).is_some())
}

/// Runs a shell script with the macOS administrator prompt.
fn run_as_admin(script: &str) -> Result<(), String> {
    let dir = std::env::temp_dir().join("cleanyou-admin");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let file = dir.join("run.sh");
    std::fs::write(&file, script).map_err(|e| e.to_string())?;
    let cmd = format!("/bin/sh '{}'", file.display());
    let r = crate::system::admin(&cmd);
    let _ = std::fs::remove_file(&file);
    r
}

pub fn install_helper() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key><array><string>{HELPER}</string><string>--charge-daemon</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
</dict></plist>
"#
    );
    let tmp_plist = std::env::temp_dir().join("cleanyou-admin").join("charge.plist");
    std::fs::create_dir_all(tmp_plist.parent().unwrap()).map_err(|e| e.to_string())?;
    std::fs::write(&tmp_plist, plist).map_err(|e| e.to_string())?;
    let _ = std::fs::create_dir_all(shared_dir());
    let script = format!(
        "set -e
launchctl bootout system/{LABEL} 2>/dev/null || true
mkdir -p '/Library/Application Support/CleanYou'
cp '{exe}' '{HELPER}'
chown root:wheel '{HELPER}'
chmod 755 '{HELPER}'
cp '{plist}' '{dest}'
chown root:wheel '{dest}'
chmod 644 '{dest}'
launchctl bootstrap system '{dest}'
",
        exe = exe.display(),
        plist = tmp_plist.display(),
        dest = daemon_plist_path(),
    );
    run_as_admin(&script)
}

pub fn uninstall_helper() -> Result<(), String> {
    let script = format!(
        "launchctl bootout system/{LABEL} 2>/dev/null || true
'{HELPER}' --charge-reset || true
rm -f '{dest}' '{HELPER}'
",
        dest = daemon_plist_path()
    );
    run_as_admin(&script)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(pct: u32, temp: f32) -> Battery {
        Battery { pct, temp_c: temp, ..Default::default() }
    }

    fn run(cfg: &ChargeConf, b: &Battery, charging: bool) -> Plan {
        plan(cfg, b, charging, &mut Calib::default(), (0, 12))
    }

    #[test]
    fn limit_and_sailing() {
        let cfg = ChargeConf { enabled: true, ..Default::default() };
        assert!(!run(&cfg, &b(80, 30.0), true).charge, "stops at the limit");
        assert!(!run(&cfg, &b(78, 30.0), false).charge, "sails just below");
        assert!(run(&cfg, &b(75, 30.0), false).charge, "restarts at limit - sail");
        assert!(run(&cfg, &b(79, 30.0), true).charge, "keeps charging up to the limit");
        assert!(!run(&cfg, &b(50, 36.0), true).charge, "heat guard");
        let off = ChargeConf::default();
        assert!(run(&off, &b(95, 30.0), false).charge, "disabled = normal charging");
        let topup = ChargeConf { topup_until: now() + 60, ..cfg.clone() };
        assert!(run(&topup, &b(90, 30.0), false).charge, "top up passes the limit");
    }

    #[test]
    fn discharge_only_when_plugged_and_above_limit() {
        let cfg = ChargeConf { enabled: true, discharge: true, ..Default::default() };
        let mut plugged = b(95, 30.0);
        plugged.external = true;
        assert!(!run(&cfg, &plugged, false).adapter, "runs on battery down to the limit");
        plugged.pct = 80;
        assert!(run(&cfg, &plugged, false).adapter, "adapter back on at the limit");
        assert!(run(&cfg, &b(95, 30.0), false).adapter, "unplugged: never touch the adapter");
    }

    #[test]
    fn schedule() {
        let cfg = ChargeConf { enabled: true, sched_days: 1, sched_hour: 8, ..Default::default() };
        assert!(plan(&cfg, &b(80, 30.0), false, &mut Calib::default(), (0, 6)).charge, "Monday 6am charges to 100");
        assert!(!plan(&cfg, &b(80, 30.0), false, &mut Calib::default(), (1, 6)).charge, "Tuesday not scheduled");
        assert!(!plan(&cfg, &b(80, 30.0), false, &mut Calib::default(), (0, 9)).charge, "after the hour");
    }

    #[test]
    fn calibration_steps() {
        let cfg = ChargeConf { enabled: true, calibrate_since: 42, ..Default::default() };
        let mut c = Calib::default();
        assert!(plan(&cfg, &b(60, 30.0), false, &mut c, (0, 12)).charge);
        assert_eq!(c.stage, 1);
        let p = plan(&cfg, &b(100, 30.0), true, &mut c, (0, 12));
        assert_eq!((c.stage, p.charge, p.adapter), (2, false, false));
        plan(&cfg, &b(15, 30.0), false, &mut c, (0, 12));
        assert_eq!(c.stage, 3);
        plan(&cfg, &b(100, 30.0), true, &mut c, (0, 12));
        assert_eq!(c.stage, 4, "finished");
        assert!(!plan(&cfg, &b(100, 30.0), true, &mut c, (0, 12)).charge, "back to the limit");
    }
}
