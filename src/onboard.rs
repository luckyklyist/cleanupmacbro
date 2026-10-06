//! First-launch welcome tour: what Clean You does, Full Disk Access, and the first scan.

use super::*;
use std::path::Path;

const STEPS: usize = 4;

#[derive(Default)]
pub struct State {
    step: usize,
    icon: Option<egui::TextureHandle>,
    fda: bool,
    fda_at: Option<Instant>,
    move_err: Option<String>,
}

fn marker() -> PathBuf {
    agent::app_dir().join("onboarded")
}

/// True until the tour has been finished or skipped once.
pub fn needed() -> bool {
    !marker().exists()
}

fn finish() {
    let _ = std::fs::create_dir_all(agent::app_dir());
    let _ = std::fs::write(marker(), updates::VERSION);
}

/// The TCC database is only readable with Full Disk Access.
fn has_fda() -> bool {
    std::fs::File::open("/Library/Application Support/com.apple.TCC/TCC.db").is_ok()
}

/// The running app bundle, if launched from one.
fn bundle() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    exe.ancestors().find(|p| p.extension().is_some_and(|e| e == "app")).map(Path::to_path_buf)
}

/// Copies the app from the disk image into /Applications and reopens it from there.
fn move_to_applications(app: &Path) -> Result<(), String> {
    let dest = Path::new("/Applications/Clean You.app");
    let _ = trash::delete(dest);
    let ok = std::process::Command::new("/usr/bin/ditto").arg(app).arg(dest).status().map_err(|e| e.to_string())?;
    if !ok.success() {
        return Err("Couldn't copy to Applications. Drag it there in Finder instead.".into());
    }
    let _ = std::process::Command::new("/usr/bin/open").args(["-n"]).arg(dest).spawn();
    std::process::exit(0);
}

fn feature(ui: &mut Ui, icon: &str, color: Color32, title: &str, text: &str) {
    Frame::none().fill(CARD).rounding(12.0).stroke(Stroke::new(1.0_f32, BORDER)).inner_margin(14.0).show(ui, |ui| {
        ui.set_width(250.0);
        ui.set_height(64.0);
        ui.horizontal(|ui| {
            badge(ui, icon, color, 38.0);
            ui.vertical(|ui| {
                ui.label(RichText::new(title).strong().color(Color32::WHITE));
                ui.label(RichText::new(text).small().color(DIM));
            });
        });
    });
}

fn dots(ui: &mut Ui, step: usize) {
    let w = STEPS as f32 * 16.0;
    let (rect, _) = ui.allocate_exact_size(vec2(w, 8.0), Sense::hover());
    for i in 0..STEPS {
        let c = pos2(rect.left() + 4.0 + i as f32 * 16.0, rect.center().y);
        if i == step {
            ui.painter().rect_filled(Rect::from_center_size(c + vec2(4.0, 0.0), vec2(16.0, 8.0)), 4.0, ACCENT);
        } else {
            ui.painter().circle_filled(c, 3.5, TRACK);
        }
    }
}

impl App {
    /// Draws the tour over the whole window. Returns once it's finished or skipped.
    pub(crate) fn onboarding(&mut self, ctx: &egui::Context) {
        let Some(st) = self.onboarding.as_mut() else { return };
        let icon = st
            .icon
            .get_or_insert_with(|| {
                let img = image::load_from_memory(include_bytes!("../assets/app-icon.png")).expect("valid icon").to_rgba8();
                let size = [img.width() as usize, img.height() as usize];
                ctx.load_texture("onboard-icon", egui::ColorImage::from_rgba_unmultiplied(size, &img), Default::default())
            })
            .clone();
        if st.step == 2 && st.fda_at.map_or(true, |t| t.elapsed() > Duration::from_secs(1)) {
            st.fda = has_fda();
            st.fda_at = Some(Instant::now());
            ctx.request_repaint_after(Duration::from_secs(1));
        }
        let (mut next, mut back, mut done, mut scan) = (false, false, false, false);
        egui::CentralPanel::default().frame(Frame::none().fill(BG).inner_margin(40.0)).show(ctx, |ui| {
            let st = self.onboarding.as_mut().unwrap();
            ui.with_layout(Layout::top_down(Align::Center), |ui| {
                ui.add_space((ui.available_height() - 470.0).max(0.0) / 2.0);
                ui.allocate_ui_with_layout(vec2(560.0, 400.0), Layout::top_down(Align::Center), |ui| match st.step {
                    0 => {
                        ui.add(egui::Image::new(&icon).fit_to_exact_size(vec2(120.0, 120.0)));
                        ui.add_space(14.0);
                        ui.label(RichText::new("Welcome to Clean You").size(30.0).strong().color(Color32::WHITE));
                        ui.label(RichText::new(format!("Version {}", updates::VERSION)).color(DIM));
                        ui.add_space(12.0);
                        ui.label(
                            RichText::new("A fast, free and private Mac cleaner. Reclaim space, remove apps completely and look after your battery.")
                                .size(15.0)
                                .color(DIM),
                        );
                        if let Some(app) = bundle().filter(|p| p.starts_with("/Volumes")) {
                            ui.add_space(18.0);
                            Frame::none().fill(WARN.gamma_multiply(0.12)).rounding(12.0).inner_margin(14.0).show(ui, |ui| {
                                ui.set_width(460.0);
                                ui.label(RichText::new(format!("{}  You're running Clean You from the disk image", ic::WARNING)).strong().color(WARN));
                                ui.label(RichText::new("Move it to Applications so it keeps working after you eject.").small().color(DIM));
                                ui.add_space(6.0);
                                if primary(ui, true, format!("{}  Move to Applications", ic::FOLDER_SIMPLE), WARN).clicked() {
                                    st.move_err = move_to_applications(&app).err();
                                }
                                if let Some(e) = &st.move_err {
                                    ui.label(RichText::new(e).small().color(DANGER));
                                }
                            });
                        }
                    }
                    1 => {
                        ui.label(RichText::new("Everything in one place").size(26.0).strong().color(Color32::WHITE));
                        ui.label(RichText::new("A few of the things Clean You can do.").color(DIM));
                        ui.add_space(18.0);
                        egui::Grid::new("features").spacing(vec2(12.0, 12.0)).show(ui, |ui| {
                            feature(ui, ic::BROOM, WARN, "Smart Clean", "Caches, logs and dev junk");
                            feature(ui, ic::CHART_DONUT, Color32::from_rgb(100, 210, 255), "Disk Map", "See what fills any drive");
                            ui.end_row();
                            feature(ui, ic::PACKAGE, Color32::from_rgb(94, 92, 230), "Uninstaller", "Apps and their leftovers");
                            feature(ui, ic::COPY, Color32::from_rgb(255, 214, 10), "Duplicates", "Identical files, one kept");
                            ui.end_row();
                            feature(ui, ic::BATTERY_CHARGING, SUCCESS, "Battery Care", "80% limit and heat guard");
                            feature(ui, ic::EYE_SLASH, Color32::from_rgb(255, 55, 95), "Privacy", "Browser history and cookies");
                            ui.end_row();
                        });
                    }
                    2 => {
                        badge(ui, ic::SHIELD_CHECK, if st.fda { SUCCESS } else { ACCENT }, 84.0);
                        ui.add_space(14.0);
                        ui.label(RichText::new("Give Full Disk Access").size(26.0).strong().color(Color32::WHITE));
                        ui.add_space(6.0);
                        ui.label(
                            RichText::new("Optional, but it lets Clean You see Safari data, Mail downloads and other apps' containers for a complete scan.")
                                .color(DIM),
                        );
                        ui.add_space(16.0);
                        if st.fda {
                            ui.label(RichText::new(format!("{}  Full Disk Access is on", ic::CHECK_CIRCLE)).size(16.0).strong().color(SUCCESS));
                        } else {
                            Frame::none().fill(CARD).rounding(12.0).stroke(Stroke::new(1.0_f32, BORDER)).inner_margin(16.0).show(ui, |ui| {
                                ui.set_width(440.0);
                                ui.with_layout(Layout::top_down(Align::Min), |ui| {
                                    for (n, t) in [
                                        "Click Open Settings below.",
                                        "Turn on Clean You (or press + and pick it from Applications).",
                                        "Come back here. macOS may ask to reopen the app.",
                                    ]
                                    .iter()
                                    .enumerate()
                                    {
                                        ui.label(RichText::new(format!("{}.  {t}", n + 1)).color(Color32::WHITE));
                                    }
                                });
                            });
                            ui.add_space(12.0);
                            if primary(ui, true, format!("{}  Open Settings", ic::GEAR), ACCENT).clicked() {
                                system::open("x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles");
                            }
                        }
                    }
                    _ => {
                        badge(ui, ic::SPARKLE, SUCCESS, 84.0);
                        ui.add_space(14.0);
                        ui.label(RichText::new("You're all set").size(28.0).strong().color(Color32::WHITE));
                        ui.add_space(6.0);
                        ui.label(RichText::new("Nothing is removed without your say. Files go to the Trash first, so you can always undo.").color(DIM));
                        ui.add_space(22.0);
                        let b = Button::new(RichText::new(format!("{}  Run my first scan", ic::MAGNIFYING_GLASS)).size(16.0).strong().color(Color32::WHITE))
                            .fill(ACCENT)
                            .rounding(22.0)
                            .min_size(vec2(220.0, 46.0));
                        if ui.add(b).clicked() {
                            scan = true;
                        }
                        ui.add_space(8.0);
                        if ui.link(RichText::new("Take me to the dashboard").color(DIM)).clicked() {
                            done = true;
                        }
                    }
                });
                ui.add_space(20.0);
                dots(ui, st.step);
                ui.add_space(18.0);
                if st.step < STEPS - 1 {
                    ui.allocate_ui_with_layout(vec2(560.0, 36.0), Layout::left_to_right(Align::Center), |ui| {
                        if st.step > 0 {
                            back = soft(ui, true, format!("{}  Back", ic::ARROW_LEFT), DIM).clicked();
                        } else {
                            done = ui.link(RichText::new("Skip").color(DIM)).clicked();
                        }
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            let label = if st.step == 2 && !st.fda { "Later" } else { "Continue" };
                            next = primary(ui, true, format!("{label}  {}", ic::ARROW_RIGHT), ACCENT).clicked();
                        });
                    });
                }
            });
        });
        let st = self.onboarding.as_mut().unwrap();
        if ctx.input(|i| i.key_pressed(egui::Key::Enter)) && st.step < STEPS - 1 {
            next = true;
        }
        if next {
            st.step += 1;
        }
        if back {
            st.step = st.step.saturating_sub(1);
        }
        if done || scan {
            finish();
            self.onboarding = None;
            self.page = Page::Dashboard;
        }
        if scan {
            self.start_scan();
        }
    }
}
