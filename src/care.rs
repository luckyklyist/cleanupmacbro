//! Battery Care and Smart Alerts pages.

use super::*;
use crate::{agent, battery};

const GREEN: Color32 = Color32::from_rgb(48, 209, 88);
const PINK: Color32 = Color32::from_rgb(255, 55, 95);

/// iOS-style on/off switch.
fn toggle(ui: &mut Ui, on: &mut bool) -> Response {
    let (rect, mut resp) = ui.allocate_exact_size(vec2(42.0, 24.0), Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let t = ui.ctx().animate_bool(resp.id, *on);
    let p = ui.painter();
    p.rect_filled(rect, 12.0, if *on { GREEN } else { TRACK });
    let x = rect.left() + 12.0 + t * (rect.width() - 24.0);
    p.circle_filled(pos2(x, rect.center().y), 9.5, Color32::WHITE);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A settings row: icon, title, description, then a switch on the right.
fn setting(ui: &mut Ui, icon: &str, color: Color32, title: &str, desc: &str, on: &mut bool) -> bool {
    let mut changed = false;
    ui.allocate_ui_with_layout(vec2(ui.available_width(), 46.0), Layout::right_to_left(Align::Center), |ui| {
        changed = toggle(ui, on).changed();
        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
            badge(ui, icon, color, 34.0);
            ui.vertical(|ui| {
                ui.label(RichText::new(title).strong().color(Color32::WHITE));
                ui.add(Label::new(RichText::new(desc).small().color(DIM)).truncate());
            });
        });
    });
    changed
}

fn indent(ui: &mut Ui, add: impl FnOnce(&mut Ui)) {
    ui.horizontal(|ui| {
        ui.add_space(44.0);
        add(ui);
    });
}

impl App {
    fn spawn_task(&self, label: &'static str, f: fn() -> Result<(), String>) {
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let msg = match f() {
                Ok(()) => format!("{label}: done"),
                Err(e) if e.contains("-128") => format!("{label}: cancelled"),
                Err(e) => format!("{label}: {e}"),
            };
            let _ = tx.send(Msg::Status(msg));
        });
    }

    fn set_agent(&mut self, on: bool) {
        if on {
            match agent::install_agent() {
                Ok(()) => self.toast("Background monitor is on", true),
                Err(e) => self.toast(e, false),
            }
        } else {
            agent::uninstall_agent();
            self.toast("Background monitor turned off", true);
        }
        self.agent_on = agent::agent_installed();
    }

    /// Shown when a feature needs the background monitor but it's off.
    fn needs_agent(&mut self, ui: &mut Ui) {
        if self.agent_on {
            return;
        }
        ui.add_space(6.0);
        Frame::none().fill(WARN.gamma_multiply(0.12)).rounding(10.0).inner_margin(10.0).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("{} Reminders and alerts need the background monitor.", ic::WARNING)).color(WARN));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.button("Turn on").clicked() {
                        self.set_agent(true);
                    }
                });
            });
        });
    }

    pub(crate) fn battery_page(&mut self, ui: &mut Ui) {
        header(ui, ic::BATTERY_CHARGING, GREEN, "Battery Care", "Keep your battery healthy for years with a charge limit, reminders and health info.", |_| {});
        let Some(b) = self.batt.clone() else {
            card(ui, |ui| ui.label(RichText::new("No battery found on this Mac.").color(DIM)));
            return;
        };
        let before_charge = self.charge.clone();
        let before_settings = self.settings.clone();

        egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
            // ----- status cards -----
            let helper_reason = if self.helper.alive { self.helper.reason.clone() } else { String::new() };
            ui.columns(3, |cols| {
                card(&mut cols[0], |ui| {
                    ui.set_min_height(130.0);
                    ui.label(RichText::new(format!("{}  Charge", ic::LIGHTNING)).color(DIM));
                    ui.horizontal(|ui| {
                        let c = if b.pct <= 20 { DANGER } else { GREEN };
                        ring(ui, b.pct as f32 / 100.0, c, 86.0, &format!("{}%", b.pct));
                        ui.vertical(|ui| {
                            ui.add_space(14.0);
                            let (title, sub) = if b.charging {
                                ("Charging".to_string(), b.watts.map(|w| format!("{w} W adapter")).unwrap_or_default())
                            } else if b.external {
                                ("Plugged in".to_string(), if helper_reason.is_empty() { "Not charging".into() } else { helper_reason.clone() })
                            } else {
                                let left = b.minutes.map(|m| format!("{}h {:02}m left", m / 60, m % 60)).unwrap_or("Calculating…".into());
                                ("On battery".to_string(), left)
                            };
                            ui.label(RichText::new(title).size(17.0).strong().color(Color32::WHITE));
                            ui.label(RichText::new(sub).small().color(DIM));
                        });
                    });
                });
                card(&mut cols[1], |ui| {
                    ui.set_min_height(130.0);
                    ui.label(RichText::new(format!("{}  Health", ic::HEARTBEAT)).color(DIM));
                    ui.horizontal(|ui| {
                        let c = if b.health >= 0.8 { GREEN } else if b.health >= 0.7 { WARN } else { DANGER };
                        ring(ui, b.health, c, 86.0, &format!("{:.0}%", b.health * 100.0));
                        ui.vertical(|ui| {
                            ui.add_space(10.0);
                            let cond = if b.health >= 0.8 { "Normal" } else { "Service recommended" };
                            ui.label(RichText::new(cond).size(17.0).strong().color(Color32::WHITE));
                            ui.label(RichText::new(format!("{} cycles", b.cycles)).small().color(DIM));
                            ui.label(RichText::new(format!("{} / {} mAh", b.max_mah, b.design_mah)).small().color(DIM));
                        });
                    });
                });
                card(&mut cols[2], |ui| {
                    ui.set_min_height(130.0);
                    ui.label(RichText::new(format!("{}  Temperature", ic::THERMOMETER)).color(DIM));
                    let (c, word) = if b.temp_c < 35.0 { (GREEN, "Cool") } else if b.temp_c < 40.0 { (WARN, "Warm") } else { (DANGER, "Hot") };
                    ui.add_space(8.0);
                    ui.label(RichText::new(format!("{:.1} °C", b.temp_c)).size(30.0).strong().color(Color32::WHITE));
                    ui.label(RichText::new(word).color(c));
                    bar(ui, (b.temp_c / 50.0).clamp(0.0, 1.0), c);
                });
            });
            ui.add_space(16.0);

            // ----- charge limit -----
            card(ui, |ui| {
                let mut on = self.charge.enabled;
                let title = format!("Stop charging at {}%", self.charge.limit);
                if setting(ui, ic::BATTERY_PLUS, GREEN, &title, "Keeping the battery between 20% and 80% can roughly double its lifespan.", &mut on) {
                    self.charge.enabled = on;
                    if on && !battery::helper_installed() {
                        self.toast("Installing the charge helper. macOS will ask for your password", true);
                        self.spawn_task("Charge limit helper", battery::install_helper);
                    }
                }
                if !self.charge_supported {
                    ui.label(RichText::new(format!("{} This Mac doesn't expose charge control.", ic::WARNING)).color(WARN));
                    return;
                }
                if !self.charge.enabled {
                    return;
                }
                ui.add_space(4.0);
                indent(ui, |ui| {
                    ui.label(RichText::new("Limit").color(DIM));
                    ui.add(egui::Slider::new(&mut self.charge.limit, 50..=100).step_by(5.0).suffix("%"));
                });
                indent(ui, |ui| {
                    ui.label(RichText::new("Sailing").color(DIM)).on_hover_text("Lets the charge drift a little below the limit before charging again, so the battery isn't topped up constantly.");
                    ui.add(egui::Slider::new(&mut self.charge.sail, 1..=20).suffix("%"));
                    let floor = self.charge.limit.saturating_sub(self.charge.sail);
                    ui.label(RichText::new(format!("resume charging at {floor}%")).small().color(DIM));
                });
                indent(ui, |ui| {
                    ui.checkbox(&mut self.charge.heat_guard, "Pause charging while the battery is warm (35 °C+)");
                });
                indent(ui, |ui| {
                    ui.checkbox(&mut self.charge.discharge, "Discharge to the limit while plugged in")
                        .on_hover_text("Plugged in at 95%? Your Mac runs on battery until it's back at the limit, then uses the charger.");
                });
                indent(ui, |ui| {
                    ui.checkbox(&mut self.charge.led, "MagSafe light: orange while charging, green at the limit");
                });
                indent(ui, |ui| {
                    let mut on = self.charge.sched_days != 0;
                    if ui.checkbox(&mut on, "Charge to 100% by").changed() {
                        self.charge.sched_days = if on { 0b0011111 } else { 0 };
                    }
                    if on {
                        ui.add(egui::Slider::new(&mut self.charge.sched_hour, 4..=23).suffix(":00"));
                        for (i, d) in ["M", "T", "W", "T", "F", "S", "S"].iter().enumerate() {
                            let bit = 1u8 << i;
                            let sel = self.charge.sched_days & bit != 0;
                            if ui.selectable_label(sel, *d).clicked() {
                                self.charge.sched_days ^= bit;
                            }
                        }
                    }
                });
                indent(ui, |ui| {
                    if self.charge.topping_up() {
                        ui.label(RichText::new(format!("{} Topping up to 100% for the next few hours", ic::LIGHTNING)).color(WARN));
                        if ui.small_button("Cancel").clicked() {
                            self.charge.topup_until = 0;
                        }
                    } else if ui.button(format!("{}  Top up to 100% once", ic::LIGHTNING)).on_hover_text("For a long trip. Goes back to the limit after 8 hours").clicked() {
                        self.charge.start_topup();
                    }
                });
                ui.add_space(6.0);
                indent(ui, |ui| {
                    let (icon, c, text) = if self.helper.alive {
                        let state = if !self.helper.adapter {
                            "running on battery"
                        } else if self.helper.charging_allowed {
                            "charging allowed"
                        } else {
                            "charging paused"
                        };
                        (ic::CHECK_CIRCLE, GREEN, format!("Helper running · {} · {state}", self.helper.reason))
                    } else if battery::helper_installed() {
                        (ic::CIRCLE_NOTCH, WARN, "Helper starting…".to_string())
                    } else {
                        (ic::WARNING, WARN, "Helper not installed".to_string())
                    };
                    ui.label(RichText::new(format!("{icon} {text}")).small().color(c));
                    if !battery::helper_installed() && ui.small_button("Install").clicked() {
                        self.spawn_task("Charge limit helper", battery::install_helper);
                    }
                });
                indent(ui, |ui| {
                    if ui.link("Turn off Apple's Optimized Battery Charging (it fights the limit)").clicked() {
                        system::open("x-apple.systempreferences:com.apple.Battery-Settings.extension");
                    }
                });
            });
            if battery::helper_installed() && !self.charge.enabled {
                ui.horizontal(|ui| {
                    ui.add_space(8.0);
                    if ui.link(RichText::new("Uninstall charge helper").small().color(DIM)).clicked() {
                        self.spawn_task("Uninstall helper", battery::uninstall_helper);
                    }
                });
            }
            ui.add_space(12.0);

            // ----- calibration -----
            card(ui, |ui| {
                ui.allocate_ui_with_layout(vec2(ui.available_width(), 46.0), Layout::right_to_left(Align::Center), |ui| {
                    let running = self.charge.calibrate_since != 0 && self.helper.calib_stage != 4;
                    if running {
                        if ui.button("Stop").clicked() {
                            self.charge.calibrate_since = 0;
                        }
                    } else if ui.add_enabled(battery::helper_installed(), Button::new("Start")).on_disabled_hover_text("Turn on the charge limit first to install the helper").clicked() {
                        self.charge.start_calibration();
                        self.toast("Calibration started. Keep your charger plugged in", true);
                    }
                    ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                        badge(ui, ic::ARROWS_CLOCKWISE, ACCENT, 34.0);
                        ui.vertical(|ui| {
                            ui.label(RichText::new("Calibrate battery").strong().color(Color32::WHITE));
                            let sub = match (self.charge.calibrate_since != 0, self.helper.calib_stage) {
                                (true, 1..=3) => self.helper.reason.clone(),
                                (true, 4) => "Done. The percentage and health readings are accurate again.".into(),
                                (true, _) => "Starting…".into(),
                                _ => "Charges to 100%, runs down to 15%, then charges again. Do it about once a month.".into(),
                            };
                            ui.add(Label::new(RichText::new(sub).small().color(DIM)).truncate());
                        });
                    });
                });
            });
            ui.add_space(12.0);

            // ----- reminders -----
            card(ui, |ui| {
                let title = format!("Remind me at {}%", self.settings.low_pct);
                setting(ui, ic::BATTERY_WARNING, DANGER, &title, "A notification when the battery runs low.", &mut self.settings.low_reminder);
                if self.settings.low_reminder {
                    indent(ui, |ui| {
                        ui.label(RichText::new("Threshold").color(DIM));
                        ui.add(egui::Slider::new(&mut self.settings.low_pct, 5..=40).suffix("%"));
                    });
                }
                ui.separator();
                setting(
                    ui,
                    ic::LOCK,
                    WARN,
                    "Full-screen reminder until I plug in",
                    "Covers the screen at the threshold. Disappears when the charger is connected.",
                    &mut self.settings.lock_screen,
                );
                indent(ui, |ui| {
                    if ui.small_button(format!("{} Preview", ic::EYE)).clicked() {
                        if let Ok(exe) = std::env::current_exe() {
                            let _ = std::process::Command::new(exe).args(["--plug-in", "--preview"]).spawn();
                        }
                    }
                });
                ui.separator();
                setting(ui, ic::PLUG, GREEN, "Tell me when it's charged", "A notification when you reach the limit (or 100%).", &mut self.settings.full_alert);
                self.needs_agent(ui);
            });
            ui.add_space(12.0);

            // ----- tips -----
            card(ui, |ui| {
                ui.label(RichText::new(format!("{}  Make your battery last longer", ic::LIGHTBULB)).strong().color(Color32::WHITE));
                ui.add_space(4.0);
                for tip in [
                    "Stay between 20% and 80% day to day. Use the charge limit when you work plugged in.",
                    "Heat is the #1 battery killer. Don't charge under a blanket or in direct sun.",
                    "Once a month, top up to 100% so macOS can recalibrate the percentage.",
                    "Use the original or a quality USB-C charger with enough watts for your Mac.",
                    "Storing it for weeks? Leave it around 50%, not full or empty.",
                ] {
                    ui.label(RichText::new(format!("•  {tip}")).color(DIM));
                }
            });
        });

        if self.charge != before_charge {
            if let Err(e) = self.charge.save() {
                self.toast(format!("Couldn't save charge settings: {e}"), false);
            }
        }
        if self.settings != before_settings {
            self.settings.save();
        }
    }

    pub(crate) fn alerts_page(&mut self, ui: &mut Ui) {
        header(ui, ic::BELL, PINK, "Smart Alerts", "Get a heads-up when something slows your Mac down or wears out the battery.", |_| {});
        let before = self.settings.clone();

        egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
            card(ui, |ui| {
                let mut on = self.agent_on;
                if setting(
                    ui,
                    ic::PULSE,
                    ACCENT,
                    "Background monitor",
                    "Starts at login and watches quietly (about 5 MB of RAM). Alerts work even when this window is closed.",
                    &mut on,
                ) {
                    self.set_agent(on);
                }
                indent(ui, |ui| {
                    let running = self.agent_on && agent::agent_running();
                    let (c, t) = if running { (GREEN, "Running") } else if self.agent_on { (WARN, "Starting…") } else { (DIM, "Off") };
                    ui.label(RichText::new(format!("● {t}")).small().color(c));
                    if ui.small_button(format!("{} Test notification", ic::BELL)).clicked() {
                        std::thread::spawn(|| system::notify("Notifications work", "You'll see alerts like this one."));
                    }
                });
            });
            ui.add_space(12.0);

            // ----- automation -----
            let mut restart = false;
            card(ui, |ui| {
                let s = &mut self.settings;
                restart |= setting(ui, ic::BATTERY_HIGH, GREEN, "Menu bar icon",
                    "Battery %, charge limit, keep awake and Smart Clean one click away.", &mut s.menu_bar);
                ui.separator();
                let mut auto = s.auto_clean != 0;
                if setting(ui, ic::BROOM, WARN, "Auto-clean junk", "Removes caches and logs on a schedule (never the Trash).", &mut auto) {
                    s.auto_clean = if auto { 2 } else { 0 };
                }
                if s.auto_clean != 0 {
                    indent(ui, |ui| {
                        ui.selectable_value(&mut s.auto_clean, 1, "Daily");
                        ui.selectable_value(&mut s.auto_clean, 2, "Weekly");
                    });
                }
                ui.separator();
                setting(ui, ic::CHART_BAR, ACCENT, "Weekly report", "A notification every week with space freed, free space and battery health.", &mut s.weekly_report);
            });
            if restart && self.agent_on {
                self.settings.save();
                self.set_agent(true);
            }
            ui.add_space(12.0);

            card(ui, |ui| {
                let s = &mut self.settings;
                setting(ui, ic::MEMORY, Color32::from_rgb(191, 90, 242), &format!("An app uses more than {:.1} GB of RAM", s.ram_gb),
                    "Names the app so you can quit or restart it.", &mut s.ram_alert);
                if s.ram_alert {
                    indent(ui, |ui| {
                        ui.add(egui::Slider::new(&mut s.ram_gb, 1.0..=8.0).step_by(0.5).suffix(" GB"));
                    });
                }
                ui.separator();
                setting(ui, ic::GAUGE, WARN, "Memory is almost full", "When more than 92% of RAM is in use.", &mut s.pressure_alert);
                ui.separator();
                setting(ui, ic::CPU, ACCENT, &format!("An app stays above {:.0}% CPU", s.cpu_pct),
                    "For about a minute. 100% = one full core.", &mut s.cpu_alert);
                if s.cpu_alert {
                    indent(ui, |ui| {
                        ui.add(egui::Slider::new(&mut s.cpu_pct, 50.0..=400.0).step_by(10.0).suffix("%"));
                    });
                }
                ui.separator();
                setting(ui, ic::HARD_DRIVE, ACCENT, &format!("Less than {} GB free", s.disk_gb), "macOS slows down when the disk is nearly full.", &mut s.disk_alert);
                if s.disk_alert {
                    indent(ui, |ui| {
                        ui.add(egui::Slider::new(&mut s.disk_gb, 5..=50).suffix(" GB"));
                    });
                }
                ui.separator();
                setting(ui, ic::THERMOMETER, DANGER, "Battery is too hot", "Above 40 °C, which permanently wears the battery.", &mut s.heat_alert);
                ui.separator();
                setting(ui, ic::BATTERY_WARNING, DANGER, &format!("Battery at {}%", s.low_pct), "Threshold and full-screen reminder are in Battery Care.", &mut s.low_reminder);
                self.needs_agent(ui);
            });
        });

        if self.settings != before {
            self.settings.save();
        }
    }
}
