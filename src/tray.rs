//! Menu bar icon (runs inside the background monitor): live battery %, quick toggles for the
//! charge limit, keep awake and Smart Clean.

use crate::{agent, battery, scan, system};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};
use winit::application::ApplicationHandler;
use winit::event::{StartCause, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};
use winit::window::WindowId;

pub enum Ev {
    Menu(MenuEvent),
    Refresh,
}

pub struct Tray {
    event_loop: EventLoop<Ev>,
}

impl Tray {
    /// Returns the tray and a `refresh` callback the monitor thread calls after each check.
    pub fn new() -> (Tray, impl Fn() + Send + 'static) {
        let event_loop = EventLoop::<Ev>::with_user_event()
            .with_activation_policy(ActivationPolicy::Accessory)
            .with_default_menu(false)
            .build()
            .expect("event loop");
        let menu_proxy = Mutex::new(event_loop.create_proxy());
        MenuEvent::set_event_handler(Some(move |e| {
            let _ = menu_proxy.lock().unwrap().send_event(Ev::Menu(e));
        }));
        let proxy = Mutex::new(event_loop.create_proxy());
        (Tray { event_loop }, move || {
            let _ = proxy.lock().unwrap().send_event(Ev::Refresh);
        })
    }

    pub fn run(self) -> ! {
        let mut app = App::default();
        let _ = self.event_loop.run_app(&mut app);
        std::process::exit(0)
    }
}

struct Items {
    status: MenuItem,
    health: MenuItem,
    mem: MenuItem,
    disk: MenuItem,
    limit: CheckMenuItem,
    topup: MenuItem,
    discharge: CheckMenuItem,
    awake: CheckMenuItem,
    awake_hour: MenuItem,
    clean: MenuItem,
    open: MenuItem,
    quit: MenuItem,
}

#[derive(Default)]
struct App {
    tray: Option<TrayIcon>,
    items: Option<Items>,
    junk: Arc<Mutex<Option<u64>>>,
    junk_at: Option<Instant>,
    last_icon: Option<(u32, bool)>,
}

fn item(text: &str, enabled: bool) -> MenuItem {
    MenuItem::new(text, enabled, None)
}

/// Template battery glyph (black on transparent; macOS tints it for light/dark menu bars).
fn battery_icon(pct: u32, plugged: bool) -> Option<Icon> {
    let (w, h) = (44u32, 22u32);
    let mut px = vec![0u8; (w * h * 4) as usize];
    let fill_end = 7 + 27 * pct.min(100) / 100;
    for y in 0..h {
        for x in 0..w {
            let body = (2..37).contains(&x) && (4..18).contains(&y);
            let border = body && !((4..35).contains(&x) && (6..16).contains(&y));
            let level = (7..fill_end).contains(&x) && (8..14).contains(&y);
            let nub = (37..40).contains(&x) && (8..14).contains(&y);
            // A small plug dot to the right when connected.
            let dot = plugged && (41..44).contains(&x) && (9..13).contains(&y);
            if border || level || nub || dot {
                let i = ((y * w + x) * 4) as usize;
                px[i..i + 4].copy_from_slice(&[0, 0, 0, 255]);
            }
        }
    }
    Icon::from_rgba(px, w, h).ok()
}

impl App {
    fn build(&mut self) {
        let items = Items {
            status: item("Battery", false),
            health: item("Health", false),
            mem: item("Memory", false),
            disk: item("Disk", false),
            limit: CheckMenuItem::new("Limit charging", true, false, None),
            topup: item("Top up to 100% once", true),
            discharge: CheckMenuItem::new("Discharge to limit when plugged in", true, false, None),
            awake: CheckMenuItem::new("Keep awake", true, false, None),
            awake_hour: item("Keep awake for 1 hour", true),
            clean: item("Smart Clean", true),
            open: item("Open Clean You…", true),
            quit: item("Hide menu bar icon", true),
        };
        let menu = Menu::new();
        let sep = PredefinedMenuItem::separator;
        let _ = menu.append_items(&[
            &items.status,
            &items.health,
            &items.mem,
            &items.disk,
            &sep(),
            &items.limit,
            &items.topup,
            &items.discharge,
            &sep(),
            &items.awake,
            &items.awake_hour,
            &items.clean,
            &sep(),
            &items.open,
            &items.quit,
        ]);
        self.tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_icon_as_template(true)
            .with_icon(battery_icon(100, false).unwrap())
            .with_tooltip("Clean You")
            .build()
            .ok();
        self.items = Some(items);
    }

    fn refresh(&mut self) {
        let (Some(tray), Some(it)) = (&self.tray, &self.items) else { return };
        let conf = battery::ChargeConf::load();
        let helper = battery::helper_status();
        if let Some(b) = battery::info() {
            let icon_key = (b.pct, b.external);
            if self.last_icon != Some(icon_key) {
                let _ = tray.set_icon(battery_icon(b.pct, b.external));
                self.last_icon = Some(icon_key);
            }
            tray.set_title(Some(format!("{}%", b.pct)));
            let state = if helper.alive && b.external {
                helper.reason.clone()
            } else if b.charging {
                "Charging".into()
            } else if b.external {
                "Plugged in".into()
            } else {
                b.minutes.map(|m| format!("{}h {:02}m left", m / 60, m % 60)).unwrap_or("On battery".into())
            };
            it.status.set_text(format!("Battery {}% · {state}", b.pct));
            it.health.set_text(format!("Health {:.0}% · {} cycles · {:.0} °C", b.health * 100.0, b.cycles, b.temp_c));
        }
        if let Some((used, total)) = system::memory() {
            it.mem.set_text(format!("Memory {} of {}", scan::human(used), scan::human(total)));
        }
        if let Some((free, _)) = system::disk() {
            it.disk.set_text(format!("Disk {} free", scan::human(free)));
        }
        let installed = battery::helper_installed();
        it.limit.set_text(format!("Limit charging at {}%", conf.limit));
        it.limit.set_checked(conf.enabled);
        it.limit.set_enabled(installed);
        it.topup.set_text(if conf.topping_up() { "Topping up… (click to stop)" } else { "Top up to 100% once" });
        it.topup.set_enabled(installed && conf.enabled);
        it.discharge.set_checked(conf.discharge);
        it.discharge.set_enabled(installed && conf.enabled);
        let awake = system::awake_until();
        it.awake.set_checked(awake.is_some());
        it.awake.set_text(match awake {
            Some(t) if t > 0 => format!("Keep awake (for {} more min)", t.saturating_sub(now()) / 60 + 1),
            _ => "Keep awake".into(),
        });

        // Measure junk in the background at most every 30 min.
        if self.junk_at.map_or(true, |t| t.elapsed() > Duration::from_secs(1800)) {
            self.junk_at = Some(Instant::now());
            let slot = self.junk.clone();
            std::thread::spawn(move || *slot.lock().unwrap() = Some(scan::quick_clean_size()));
        }
        let junk = *self.junk.lock().unwrap();
        it.clean.set_text(match junk {
            Some(b) => format!("Smart Clean ({})", scan::human(b)),
            None => "Smart Clean".into(),
        });
    }

    fn on_menu(&mut self, e: MenuEvent, el: &ActiveEventLoop) {
        let Some(it) = &self.items else { return };
        let mut conf = battery::ChargeConf::load();
        let id = e.id;
        if id == *it.limit.id() {
            conf.enabled = !conf.enabled;
            let _ = conf.save();
        } else if id == *it.topup.id() {
            if conf.topping_up() {
                conf.topup_until = 0;
            } else {
                conf.start_topup();
            }
            let _ = conf.save();
        } else if id == *it.discharge.id() {
            conf.discharge = !conf.discharge;
            let _ = conf.save();
        } else if id == *it.awake.id() {
            if system::awake_until().is_some() {
                system::stop_awake();
            } else {
                system::keep_awake(None);
            }
        } else if id == *it.awake_hour.id() {
            system::keep_awake(Some(3600));
        } else if id == *it.clean.id() {
            let slot = self.junk.clone();
            std::thread::spawn(move || {
                let freed = scan::quick_clean();
                *slot.lock().unwrap() = Some(0);
                system::notify("Smart Clean", &format!("Freed {}", scan::human(freed)));
            });
        } else if id == *it.open.id() {
            let path = agent::Settings::load().app_path;
            let mut cmd = std::process::Command::new("/usr/bin/open");
            if path.is_empty() { cmd.args(["-a", "Clean You"]) } else { cmd.arg(&path) };
            let _ = cmd.spawn();
        } else if id == *it.quit.id() {
            el.exit();
            return;
        }
        self.refresh();
    }
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

impl ApplicationHandler<Ev> for App {
    fn new_events(&mut self, _: &ActiveEventLoop, cause: StartCause) {
        if cause == StartCause::Init {
            self.build();
            self.refresh();
        }
    }

    fn resumed(&mut self, el: &ActiveEventLoop) {
        el.set_control_flow(ControlFlow::Wait);
    }

    fn user_event(&mut self, el: &ActiveEventLoop, ev: Ev) {
        match ev {
            Ev::Menu(e) => self.on_menu(e, el),
            Ev::Refresh => self.refresh(),
        }
    }

    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
}
