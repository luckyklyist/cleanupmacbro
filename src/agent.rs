//! Background monitor that runs at login (a user LaunchAgent): low-battery reminder,
//! full-screen "plug in" lock, and alerts for RAM/CPU hogs, heat and low disk space.

use crate::{battery, system};
use eframe::egui::{self, vec2, Align2, Color32, FontId, RichText};
use egui_phosphor::regular as ic;
use std::collections::HashMap;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const LABEL: &str = "local.cleanyou.agent";

pub fn app_dir() -> PathBuf {
    crate::scan::home().join("Library/Application Support/CleanYou")
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[derive(Clone, PartialEq)]
pub struct Settings {
    pub low_reminder: bool,
    pub low_pct: u32,
    pub lock_screen: bool,
    pub ram_alert: bool,
    pub ram_gb: f32,
    pub pressure_alert: bool,
    pub cpu_alert: bool,
    pub cpu_pct: f32,
    pub disk_alert: bool,
    pub disk_gb: u64,
    pub heat_alert: bool,
    pub full_alert: bool,
    pub menu_bar: bool,
    /// 0 = off, 1 = daily, 2 = weekly.
    pub auto_clean: u8,
    pub weekly_report: bool,
    /// Where the Clean You app lives, so the menu bar can open it.
    pub app_path: String,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            low_reminder: true,
            low_pct: 20,
            lock_screen: true,
            ram_alert: true,
            ram_gb: 3.0,
            pressure_alert: true,
            cpu_alert: true,
            cpu_pct: 150.0,
            disk_alert: true,
            disk_gb: 15,
            heat_alert: true,
            full_alert: false,
            menu_bar: true,
            auto_clean: 0,
            weekly_report: true,
            app_path: String::new(),
        }
    }
}

impl Settings {
    pub fn load() -> Self {
        let d = Settings::default();
        let text = std::fs::read_to_string(app_dir().join("settings.conf")).unwrap_or_default();
        let kv: HashMap<&str, &str> = text.lines().filter_map(|l| l.split_once('=')).collect();
        fn g<T: std::str::FromStr>(kv: &HashMap<&str, &str>, k: &str, d: T) -> T {
            kv.get(k).and_then(|v| v.trim().parse().ok()).unwrap_or(d)
        }
        Settings {
            low_reminder: g(&kv, "low_reminder", d.low_reminder),
            low_pct: g(&kv, "low_pct", d.low_pct),
            lock_screen: g(&kv, "lock_screen", d.lock_screen),
            ram_alert: g(&kv, "ram_alert", d.ram_alert),
            ram_gb: g(&kv, "ram_gb", d.ram_gb),
            pressure_alert: g(&kv, "pressure_alert", d.pressure_alert),
            cpu_alert: g(&kv, "cpu_alert", d.cpu_alert),
            cpu_pct: g(&kv, "cpu_pct", d.cpu_pct),
            disk_alert: g(&kv, "disk_alert", d.disk_alert),
            disk_gb: g(&kv, "disk_gb", d.disk_gb),
            heat_alert: g(&kv, "heat_alert", d.heat_alert),
            full_alert: g(&kv, "full_alert", d.full_alert),
            menu_bar: g(&kv, "menu_bar", d.menu_bar),
            auto_clean: g(&kv, "auto_clean", d.auto_clean),
            weekly_report: g(&kv, "weekly_report", d.weekly_report),
            app_path: g(&kv, "app_path", d.app_path),
        }
    }

    pub fn save(&self) {
        let s = format!(
            "low_reminder={}\nlow_pct={}\nlock_screen={}\nram_alert={}\nram_gb={}\npressure_alert={}\ncpu_alert={}\ncpu_pct={}\ndisk_alert={}\ndisk_gb={}\nheat_alert={}\nfull_alert={}\nmenu_bar={}\nauto_clean={}\nweekly_report={}\napp_path={}\n",
            self.low_reminder, self.low_pct, self.lock_screen, self.ram_alert, self.ram_gb, self.pressure_alert,
            self.cpu_alert, self.cpu_pct, self.disk_alert, self.disk_gb, self.heat_alert, self.full_alert,
            self.menu_bar, self.auto_clean, self.weekly_report, self.app_path
        );
        let _ = std::fs::create_dir_all(app_dir());
        let _ = std::fs::write(app_dir().join("settings.conf"), s);
    }
}

fn snoozed() -> bool {
    std::fs::read_to_string(app_dir().join("snooze")).ok().and_then(|s| s.trim().parse::<u64>().ok()).map_or(false, |t| now() < t)
}

fn snooze(minutes: u64) {
    let _ = std::fs::create_dir_all(app_dir());
    let _ = std::fs::write(app_dir().join("snooze"), (now() + minutes * 60).to_string());
}

/// Fires each alert at most once per `every` (keyed by name).
struct Cooldown(HashMap<String, Instant>);

impl Cooldown {
    fn ready(&mut self, key: &str, every: Duration) -> bool {
        match self.0.get(key) {
            Some(t) if t.elapsed() < every => false,
            _ => {
                self.0.insert(key.to_string(), Instant::now());
                true
            }
        }
    }
}

pub fn run_agent() -> ! {
    let s = Settings::load();
    if s.menu_bar {
        let (tray, refresh) = crate::tray::Tray::new();
        std::thread::spawn(move || monitor(refresh));
        tray.run();
    }
    monitor(|| {});
}

fn stamp(name: &str) -> u64 {
    std::fs::read_to_string(app_dir().join(name)).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0)
}

fn set_stamp(name: &str) {
    let _ = std::fs::write(app_dir().join(name), now().to_string());
}

/// Bytes cleaned since `since` (unix time), from the cleanup history.
pub fn freed_since(since: u64) -> u64 {
    crate::scan::history().iter().filter(|(t, _)| *t >= since).map(|(_, b)| b).sum()
}

fn monitor(on_tick: impl Fn()) -> ! {
    let mut cd = Cooldown(HashMap::new());
    let mut lock: Option<Child> = None;
    let mut cpu_strikes: HashMap<String, u32> = HashMap::new();
    let exe = std::env::current_exe().unwrap_or_default();
    let mut tick = 0u64;
    loop {
        let s = Settings::load();

        if let Some(b) = battery::info() {
            let low = !b.external && b.pct <= s.low_pct;
            if s.low_reminder && low {
                if cd.ready("low", Duration::from_secs(300)) {
                    system::notify(&format!("Battery at {}%", b.pct), "Plug in your charger now.");
                }
                let running = lock.as_mut().map_or(false, |c| matches!(c.try_wait(), Ok(None)));
                if s.lock_screen && !running && !snoozed() {
                    lock = Command::new(&exe).arg("--plug-in").spawn().ok();
                }
            }
            if !low {
                cd.0.remove("low");
            }
            if s.heat_alert && b.temp_c >= 40.0 && cd.ready("heat", Duration::from_secs(1800)) {
                system::notify(
                    &format!("Battery is hot ({:.0}°C)", b.temp_c),
                    "Heat ages batteries. Close heavy apps or let your Mac cool down.",
                );
            }
            let limit = battery::ChargeConf::load();
            let target = if limit.enabled && !limit.topping_up() { limit.limit } else { 100 };
            if s.full_alert && b.external && b.pct >= target && cd.ready("full", Duration::from_secs(3 * 3600)) {
                system::notify(&format!("Charged to {}%", b.pct), "You can unplug now.");
            }
        }

        // Apps every 30 s, disk every ~5 min.
        if tick % 2 == 0 {
            let procs = system::top_processes(15);
            for p in &procs {
                let gb = p.mem as f32 / 1e9;
                if s.ram_alert && gb >= s.ram_gb && cd.ready(&format!("ram:{}", p.name), Duration::from_secs(1800)) {
                    system::notify(
                        &format!("{} is using {:.1} GB of memory", p.name, gb),
                        "Quit or restart it to speed your Mac up.",
                    );
                }
            }
            for p in system::top_by_cpu(8) {
                let strikes = cpu_strikes.entry(p.name.clone()).or_default();
                *strikes = if p.cpu >= s.cpu_pct { *strikes + 1 } else { 0 };
                // Busy for about a minute straight.
                if s.cpu_alert && *strikes >= 3 && cd.ready(&format!("cpu:{}", p.name), Duration::from_secs(1800)) {
                    system::notify(
                        &format!("{} is using {:.0}% CPU", p.name, p.cpu),
                        "It's been busy for a while and drains your battery.",
                    );
                }
            }
            if let Some((used, total)) = system::memory() {
                if s.pressure_alert && used as f32 / total as f32 > 0.92 && cd.ready("pressure", Duration::from_secs(1800)) {
                    let top = procs.first().map(|p| format!(" {} uses the most ({:.1} GB).", p.name, p.mem as f32 / 1e9)).unwrap_or_default();
                    system::notify("Memory is almost full", &format!("Your Mac may feel slow.{top}"));
                }
            }
        }
        if tick % 20 == 0 {
            if let Some((free, _)) = system::disk() {
                if s.disk_alert && free < s.disk_gb * 1_000_000_000 && cd.ready("disk", Duration::from_secs(6 * 3600)) {
                    system::notify(
                        &format!("Only {} free", crate::scan::human(free)),
                        "Open Clean You and run Smart Clean.",
                    );
                }
            }
        }
        // Automation: auto-clean and the weekly report (checked every ~5 min).
        if tick % 20 == 5 {
            let every = match s.auto_clean {
                1 => Some(86_400),
                2 => Some(7 * 86_400),
                _ => None,
            };
            if every.is_some_and(|e| now().saturating_sub(stamp("last_autoclean")) >= e) {
                set_stamp("last_autoclean");
                let freed = crate::scan::quick_clean();
                if freed >= 50_000_000 {
                    system::notify("Auto-clean done", &format!("Removed {} of caches and logs.", crate::scan::human(freed)));
                }
            }
            if s.weekly_report && now().saturating_sub(stamp("last_report")) >= 7 * 86_400 {
                if stamp("last_report") != 0 {
                    let freed = freed_since(now() - 7 * 86_400);
                    let free = system::disk().map(|(f, _)| crate::scan::human(f)).unwrap_or_default();
                    let health = battery::info().map(|b| format!(" · battery health {:.0}%", b.health * 100.0)).unwrap_or_default();
                    system::notify("Your weekly Mac report", &format!("Freed {} this week · {free} free{health}", crate::scan::human(freed)));
                }
                set_stamp("last_report");
            }
        }
        on_tick();
        tick += 1;
        std::thread::sleep(Duration::from_secs(15));
    }
}

// ---------- LaunchAgent install (no password needed) ----------

fn plist_path() -> PathBuf {
    crate::scan::home().join(format!("Library/LaunchAgents/{LABEL}.plist"))
}

fn uid() -> u32 {
    std::fs::metadata(crate::scan::home()).map(|m| m.uid()).unwrap_or(501)
}

pub fn agent_installed() -> bool {
    plist_path().exists()
}

pub fn agent_running() -> bool {
    Command::new("/bin/launchctl")
        .args(["print", &format!("gui/{}/{LABEL}", uid())])
        .output()
        .map_or(false, |o| o.status.success())
}

pub fn install_agent() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    std::fs::create_dir_all(app_dir()).map_err(|e| e.to_string())?;
    // Remember where the app is so the menu bar's "Open Clean You" works.
    let mut s = Settings::load();
    s.app_path = exe.ancestors().find(|p| p.extension().map_or(false, |e| e == "app")).unwrap_or(&exe).display().to_string();
    s.save();
    let bin = app_dir().join("clean-you");
    let _ = std::fs::remove_file(&bin);
    std::fs::copy(&exe, &bin).map_err(|e| e.to_string())?;
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key><array><string>{}</string><string>--agent</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>
  <key>ProcessType</key><string>Background</string>
</dict></plist>
"#,
        bin.display()
    );
    let _ = std::fs::create_dir_all(plist_path().parent().unwrap());
    std::fs::write(plist_path(), plist).map_err(|e| e.to_string())?;
    let domain = format!("gui/{}", uid());
    let _ = Command::new("/bin/launchctl").args(["bootout", &format!("{domain}/{LABEL}")]).status();
    let ok = Command::new("/bin/launchctl")
        .args(["bootstrap", &domain])
        .arg(plist_path())
        .status()
        .map_or(false, |s| s.success());
    if ok { Ok(()) } else { Err("launchctl couldn't start the monitor".into()) }
}

pub fn uninstall_agent() {
    let _ = Command::new("/bin/launchctl").args(["bootout", &format!("gui/{}/{LABEL}", uid())]).status();
    let _ = std::fs::remove_file(plist_path());
    let _ = std::fs::remove_file(app_dir().join("clean-you"));
}

// ---------- full-screen "plug in your charger" lock ----------

pub struct PlugIn {
    preview: bool,
    battery: Option<battery::Battery>,
    last: Instant,
    plugged_at: Option<Instant>,
}

impl PlugIn {
    pub fn new(preview: bool) -> Self {
        PlugIn { preview, battery: battery::info(), last: Instant::now(), plugged_at: None }
    }
}

impl eframe::App for PlugIn {
    fn clear_color(&self, _: &egui::Visuals) -> [f32; 4] {
        [0.04, 0.04, 0.05, 1.0]
    }

    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        if self.last.elapsed() > Duration::from_secs(2) {
            self.battery = battery::info();
            self.last = Instant::now();
        }
        ctx.request_repaint_after(Duration::from_millis(500));
        let b = self.battery.clone().unwrap_or_default();
        if b.external && self.plugged_at.is_none() {
            self.plugged_at = Some(Instant::now());
        }
        if self.plugged_at.map_or(false, |t| t.elapsed() > Duration::from_millis(1500)) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }

        egui::CentralPanel::default().frame(egui::Frame::none().fill(Color32::from_rgb(10, 10, 13))).show(ctx, |ui| {
            let rect = ui.max_rect();
            let c = rect.center();
            let p = ui.painter();
            let (icon, color, title, sub) = if b.external {
                (ic::BATTERY_CHARGING, Color32::from_rgb(48, 209, 88), "Charging. Thank you!".to_string(), "Unlocking…".to_string())
            } else {
                (
                    ic::BATTERY_WARNING,
                    Color32::from_rgb(255, 69, 58),
                    format!("Battery at {}%", b.pct),
                    "Plug in your charger to keep using your Mac.".to_string(),
                )
            };
            p.text(c - vec2(0.0, 120.0), Align2::CENTER_CENTER, icon, FontId::proportional(120.0), color);
            p.text(c + vec2(0.0, 10.0), Align2::CENTER_CENTER, title, FontId::proportional(44.0), Color32::WHITE);
            p.text(c + vec2(0.0, 60.0), Align2::CENTER_CENTER, sub, FontId::proportional(20.0), Color32::from_rgb(160, 160, 168));

            let btn = egui::Rect::from_center_size(c + vec2(0.0, 150.0), vec2(360.0, 40.0));
            let label = if self.preview { "Close preview" } else { "I can't charge right now · snooze 10 min" };
            let r = ui.put(btn, egui::Button::new(RichText::new(label).size(15.0)).rounding(20.0));
            if r.clicked() {
                if !self.preview {
                    snooze(10);
                }
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        });
    }
}

pub fn run_plug_in(preview: bool) -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Plug in your charger")
            .with_fullscreen(true)
            .with_decorations(false)
            .with_window_level(egui::WindowLevel::AlwaysOnTop),
        ..Default::default()
    };
    eframe::run_native(
        "Clean You Plug In",
        options,
        Box::new(move |cc| {
            let mut fonts = egui::FontDefinitions::default();
            egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
            cc.egui_ctx.set_fonts(fonts);
            cc.egui_ctx.set_theme(egui::ThemePreference::Dark);
            Ok(Box::new(PlugIn::new(preview)))
        }),
    )
}
