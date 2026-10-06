mod agent;
mod battery;
mod care;
mod diskmap;
mod find;
mod inspect;
mod onboard;
mod photos;
mod privacy;
mod review;
mod startup;
mod tray;
mod uninstall;
mod scan;
mod smc;
mod system;
mod thumbs;
mod updates;
mod usage;
mod video;

use eframe::egui::{
    self, Pos2, pos2, vec2, Align, Align2, Button, Color32, FontId, Frame, Id, Label, Layout, Order, ProgressBar, Rect,
    Response, RichText, Sense, Shape, Stroke, Ui,
};
use egui_phosphor::regular as ic;
use scan::{human, Cat, Item, Msg, PHASES};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use video::{Job, Mode, Opts, Quality, Status};

const ACCENT: Color32 = Color32::from_rgb(10, 132, 255);
const DANGER: Color32 = Color32::from_rgb(255, 69, 58);
const SUCCESS: Color32 = Color32::from_rgb(48, 209, 88);
const WARN: Color32 = Color32::from_rgb(255, 159, 10);
const BG: Color32 = Color32::from_rgb(22, 22, 24);
const SIDEBAR: Color32 = Color32::from_rgb(16, 16, 18);
const CARD: Color32 = Color32::from_rgb(31, 31, 34);
const BORDER: Color32 = Color32::from_rgb(44, 44, 48);
const TRACK: Color32 = Color32::from_rgb(52, 52, 57);
const DIM: Color32 = Color32::from_rgb(146, 146, 154);
const MB: u64 = 1_000_000;

fn cat_icon(c: Cat) -> &'static str {
    match c {
        Cat::Junk => ic::BROOM,
        Cat::Dev => ic::CODE,
        Cat::Ai => ic::BRAIN,
        Cat::Large => ic::FILES,
        Cat::Duplicates => ic::COPY,
        Cat::Installers => ic::DOWNLOAD_SIMPLE,
        Cat::Games => ic::GAME_CONTROLLER,
        Cat::Recordings => ic::VIDEO_CAMERA,
    }
}

fn cat_color(c: Cat) -> Color32 {
    match c {
        Cat::Junk => Color32::from_rgb(255, 159, 10),
        Cat::Dev => Color32::from_rgb(48, 209, 88),
        Cat::Ai => Color32::from_rgb(191, 90, 242),
        Cat::Large => Color32::from_rgb(10, 132, 255),
        Cat::Duplicates => Color32::from_rgb(255, 214, 10),
        Cat::Installers => Color32::from_rgb(172, 142, 104),
        Cat::Games => Color32::from_rgb(255, 55, 95),
        Cat::Recordings => Color32::from_rgb(100, 210, 255),
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Page {
    Dashboard,
    Find,
    Review,
    Cat(Cat),
    Video,
    Speed,
    Battery,
    Alerts,
    DiskMap,
    Uninstaller,
    Privacy,
    Startup,
}

struct Confirm {
    title: String,
    /// Path and its size before deleting.
    entries: Vec<(PathBuf, u64)>,
    labels: Vec<String>,
    size: u64,
    permanent: bool,
}

struct Toast {
    text: String,
    ok: bool,
    at: Instant,
}

struct App {
    page: Page,
    items: Vec<Item>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    scanning: bool,
    scanned: bool,
    scan_started: Instant,
    scan_path: String,
    scan_files: u64,
    phases_done: Vec<&'static str>,
    min_large_mb: u64,
    query: String,
    /// Only show items untouched for at least this many days (0 = any).
    min_idle: u64,
    /// Last plain/cmd-clicked row (item index), the start of a shift-click range.
    anchor: Option<usize>,
    confirm: Option<Confirm>,
    toast: Option<Toast>,
    deleting: HashSet<PathBuf>,
    /// Every path deleted this session, so pages with their own lists can hide them.
    removed: HashSet<PathBuf>,
    diskmap: diskmap::State,
    uninstall: uninstall::State,
    privacy: privacy::State,
    startup: startup::State,
    del_total: usize,
    del_done: usize,
    freed: u64,
    del_errors: Vec<String>,
    disk: Option<(u64, u64)>,
    mem: Option<(u64, u64)>,
    sampler: system::Sampler,
    thumbs: thumbs::Thumbs,
    /// Performance page: sort apps by CPU instead of memory.
    by_cpu: bool,
    /// Cached keep-awake state (see system::awake_until).
    awake: Option<u64>,
    last_sys: Option<Instant>,
    batt: Option<battery::Battery>,
    last_batt: Option<Instant>,
    charge: battery::ChargeConf,
    helper: battery::HelperStatus,
    charge_supported: bool,
    settings: agent::Settings,
    agent_on: bool,
    jobs: Arc<Mutex<Vec<Job>>>,
    encoding: Arc<Mutex<bool>>,
    opts: Opts,
    /// Compress page shows photos instead of videos.
    photos_tab: bool,
    photo_jobs: Arc<Mutex<Vec<photos::Job>>>,
    photo_busy: Arc<Mutex<bool>>,
    photo_opts: photos::Opts,
    find: find::State,
    /// The Cleanup list: things set aside to review before they go to the Trash.
    staged: Vec<review::Staged>,
    usage: usage::History,
    updates: Option<Arc<Mutex<updates::Progress>>>,
    /// Listening ports (None until the first look).
    ports: Arc<Mutex<Option<Vec<system::Port>>>>,
    ports_at: Option<Instant>,
    /// First-launch welcome tour (None once finished).
    onboarding: Option<onboard::State>,
    self_update: Option<Arc<Mutex<updates::SelfUpdate>>>,
}

fn matches(it: &Item, cat: Cat, min_large: u64, q: &str) -> bool {
    it.cat == cat
        && (cat != Cat::Large || it.size >= min_large)
        && (q.is_empty()
            || it.label.to_lowercase().contains(q)
            || it.path.to_string_lossy().to_lowercase().contains(q))
}

impl App {
    fn new(ctx: &egui::Context) -> Self {
        let (tx, rx) = channel();
        Self {
            page: Page::Dashboard,
            items: Vec::new(),
            tx,
            rx,
            scanning: false,
            scanned: false,
            scan_started: Instant::now(),
            scan_path: String::new(),
            scan_files: 0,
            phases_done: Vec::new(),
            min_large_mb: 500,
            query: String::new(),
            min_idle: 0,
            anchor: None,
            confirm: None,
            toast: None,
            deleting: HashSet::new(),
            removed: HashSet::new(),
            diskmap: Default::default(),
            uninstall: Default::default(),
            privacy: Default::default(),
            startup: Default::default(),
            del_total: 0,
            del_done: 0,
            freed: 0,
            del_errors: Vec::new(),
            disk: system::disk(),
            mem: system::memory(),
            sampler: system::Sampler::start(ctx.clone()),
            thumbs: thumbs::Thumbs::new(ctx.clone()),
            by_cpu: false,
            awake: None,
            last_sys: None,
            batt: battery::info(),
            last_batt: Some(Instant::now()),
            charge: battery::ChargeConf::load(),
            helper: battery::helper_status(),
            charge_supported: battery::supported(),
            settings: agent::Settings::load(),
            agent_on: agent::agent_installed(),
            jobs: Arc::new(Mutex::new(Vec::new())),
            encoding: Arc::new(Mutex::new(false)),
            opts: Opts { quality: Quality::Balanced, downscale: false, replace: false },
            photos_tab: false,
            photo_jobs: Arc::new(Mutex::new(Vec::new())),
            photo_busy: Arc::new(Mutex::new(false)),
            photo_opts: photos::Opts { quality: Quality::Balanced, format: photos::Format::Heic, replace: false },
            find: Default::default(),
            staged: review::load(),
            usage: usage::History::start(),
            updates: None,
            ports: Arc::new(Mutex::new(None)),
            ports_at: None,
            onboarding: onboard::needed().then(Default::default),
            self_update: None,
        }
    }

    fn toast(&mut self, text: impl Into<String>, ok: bool) {
        self.toast = Some(Toast { text: text.into(), ok, at: Instant::now() });
    }

    fn start_scan(&mut self) {
        if self.scanning {
            return;
        }
        self.items.clear();
        self.phases_done.clear();
        self.scan_files = 0;
        self.scan_path.clear();
        self.scanning = true;
        self.scan_started = Instant::now();
        let tx = self.tx.clone();
        std::thread::spawn(move || scan::scan_all(tx));
    }

    fn min_large(&self) -> u64 {
        self.min_large_mb * MB
    }

    fn total(&self, cat: Cat) -> (usize, u64) {
        let min = self.min_large();
        self.items.iter().filter(|i| matches(i, cat, min, "")).fold((0, 0), |(n, s), i| (n + 1, s + i.size))
    }

    fn reclaimable(&self) -> u64 {
        Cat::ALL.iter().filter(|c| **c != Cat::Recordings).map(|c| self.total(*c).1).sum()
    }

    fn ask_delete(&mut self, title: impl Into<String>, idx: Vec<usize>, permanent: bool) {
        if idx.is_empty() {
            return;
        }
        let going: HashSet<&PathBuf> = idx.iter().map(|i| &self.items[*i].path).collect();
        let safe = |it: &Item| it.dup_of.as_ref().map_or(true, |o| o.exists() && !going.contains(o));
        let skipped = idx.iter().filter(|i| !safe(&self.items[**i])).count();
        let entries: Vec<_> = idx
            .iter()
            .map(|i| &self.items[*i])
            .filter(|it| safe(it))
            .map(|it| (it.path.clone(), it.label.clone(), it.size))
            .collect();
        if skipped > 0 {
            self.toast(format!("Kept {skipped} file(s): their other copy is gone, so they're no longer duplicates"), false);
        }
        self.ask_delete_paths(title, entries, permanent);
    }

    /// Opens the delete confirmation for any list of (path, label, size).
    fn ask_delete_paths(&mut self, title: impl Into<String>, entries: Vec<(PathBuf, String, u64)>, permanent: bool) {
        if entries.is_empty() {
            return;
        }
        let labels = entries.iter().map(|e| e.1.clone()).collect();
        let size = entries.iter().map(|e| e.2).sum();
        let entries = entries.into_iter().map(|(p, _, s)| (p, s)).collect();
        self.confirm = Some(Confirm { title: title.into(), entries, labels, size, permanent });
    }

    fn start_delete(&mut self, entries: Vec<(PathBuf, u64)>, permanent: bool) {
        if self.del_total > 0 {
            self.toast("Already deleting, please wait", false);
            return;
        }
        self.del_total = entries.len();
        self.del_done = 0;
        self.freed = 0;
        self.del_errors.clear();
        self.deleting = entries.iter().map(|e| e.0.clone()).collect();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            for (p, before) in entries {
                let err = scan::remove_item(&p, permanent).err();
                let remaining = if p.exists() { scan::dir_size(&p) } else { 0 };
                let _ = tx.send(Msg::Removed { path: p, err, before, remaining });
            }
            let _ = tx.send(Msg::DeleteDone);
        });
    }

    fn gather_recordings(&mut self) {
        let dest = scan::recordings_dir();
        if let Err(e) = std::fs::create_dir_all(&dest) {
            self.toast(format!("Couldn't create folder: {e}"), false);
            return;
        }
        let (mut moved, mut failed) = (0, 0);
        for it in self.items.iter_mut().filter(|i| i.cat == Cat::Recordings) {
            if it.path.parent() == Some(dest.as_path()) {
                continue;
            }
            let stem = it.path.file_stem().unwrap_or_default().to_string_lossy().to_string();
            let ext = it.path.extension().unwrap_or_default().to_string_lossy().to_string();
            let target = scan::unique(&dest, &stem, &ext);
            match std::fs::rename(&it.path, &target) {
                Ok(()) => {
                    it.path = target;
                    moved += 1;
                }
                Err(_) => failed += 1,
            }
        }
        let extra = if failed > 0 { format!(", {failed} couldn't be moved") } else { String::new() };
        self.toast(format!("Moved {moved} recordings to Movies › Screen Recordings{extra}"), failed == 0);
    }

    fn add_photos(&mut self, paths: Vec<PathBuf>) {
        let mut jobs = self.photo_jobs.lock().unwrap();
        for p in paths {
            if !jobs.iter().any(|j| j.path == p) {
                jobs.push(photos::new_job(p));
            }
        }
    }

    fn add_videos(&mut self, paths: Vec<PathBuf>) {
        let mut jobs = self.jobs.lock().unwrap();
        for p in paths {
            if !jobs.iter().any(|j| j.path == p) {
                jobs.push(video::new_job(p));
            }
        }
    }

    fn handle_messages(&mut self) {
        while let Ok(m) = self.rx.try_recv() {
            match m {
                Msg::Status(s) => self.toast(s, true),
                Msg::Progress(p, n) => {
                    self.scan_path = p;
                    self.scan_files = n;
                }
                Msg::Items(v) => self.items.extend(v),
                Msg::PhaseDone(p) => self.phases_done.push(p),
                Msg::ScanDone => {
                    self.scanning = false;
                    self.scanned = true;
                    self.items.sort_by(|a, b| b.size.cmp(&a.size));
                    self.disk = system::disk();
                    let total = human(self.reclaimable());
                    self.toast(format!("Scan complete · {total} can be cleaned"), true);
                }
                Msg::Removed { path, err, before, remaining } => {
                    self.deleting.remove(&path);
                    self.del_done += 1;
                    self.freed += before.saturating_sub(remaining);
                    if remaining < MB {
                        self.removed.insert(path.clone());
                    }
                    if let Some(i) = self.items.iter().position(|i| i.path == path) {
                        if remaining < MB {
                            self.items.remove(i);
                        } else {
                            self.items[i].size = remaining;
                            self.items[i].selected = false;
                        }
                    }
                    if let Some(e) = err {
                        self.del_errors.push(e);
                    }
                }
                Msg::DeleteDone => {
                    self.del_total = 0;
                    scan::log_freed(self.freed);
                    self.disk = system::disk();
                    let freed = human(self.freed);
                    match self.del_errors.first().cloned() {
                        None => self.toast(format!("Done · freed {freed}"), true),
                        Some(e) => {
                            let n = self.del_errors.len();
                            self.toast(format!("Freed {freed} · {n} item(s) partly skipped: {e}"), false)
                        }
                    }
                }
            }
        }
    }
}

impl App {
    /// Sidebar footer: the version, and a button that checks GitHub for a newer release.
    fn version_row(&mut self, ui: &mut Ui) {
        let state = self.self_update.as_ref().map(|s| s.lock().unwrap().clone());
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("v{}", updates::VERSION)).small().color(DIM));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                use updates::SelfUpdate::*;
                match state {
                    Some(Available(v, url)) => {
                        if primary(ui, true, format!("{}  Update to v{v}", ic::ARROW_CIRCLE_UP), SUCCESS).clicked() {
                            system::open(&url);
                        }
                    }
                    Some(Checking) => {
                        ui.add(egui::Spinner::new().size(12.0).color(DIM));
                    }
                    other => {
                        let text = match other {
                            Some(UpToDate) => format!("{} Up to date", ic::CHECK),
                            Some(Failed) => format!("{} Retry", ic::ARROW_CLOCKWISE),
                            _ => "Check for updates".into(),
                        };
                        if ui.small_button(text).on_hover_text("Asks GitHub for the latest Clean You release").clicked() {
                            let s = Arc::new(Mutex::new(updates::SelfUpdate::Checking));
                            updates::check_self(s.clone(), ui.ctx().clone());
                            self.self_update = Some(s);
                        }
                    }
                }
                if ui.small_button(ic::INFO).on_hover_text("Welcome tour").clicked() {
                    self.onboarding = Some(Default::default());
                }
            });
        });
    }
}

// ---------- small UI helpers ----------

fn card<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> egui::InnerResponse<R> {
    Frame::none()
        .fill(CARD)
        .rounding(14.0)
        .stroke(Stroke::new(1.0_f32, BORDER))
        .inner_margin(18.0)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
}

fn badge(ui: &mut Ui, icon: &str, color: Color32, size: f32) {
    let (rect, _) = ui.allocate_exact_size(vec2(size, size), Sense::hover());
    ui.painter().rect_filled(rect, size * 0.28, color.gamma_multiply(0.18));
    ui.painter().text(rect.center(), Align2::CENTER_CENTER, icon, FontId::proportional(size * 0.5), color);
}

/// Filled call-to-action button. Disabled it turns neutral gray instead of a faded color.
fn primary(ui: &mut Ui, enabled: bool, text: impl Into<String>, color: Color32) -> Response {
    let (fill, fg) = if enabled { (color, Color32::WHITE) } else { (Color32::from_rgb(42, 42, 47), Color32::from_rgb(118, 118, 126)) };
    let b = Button::new(RichText::new(text.into()).color(fg).strong())
        .fill(fill)
        .stroke(Stroke::NONE)
        .rounding(9.0)
        .min_size(vec2(0.0, 34.0))
        .sense(if enabled { Sense::click() } else { Sense::hover() });
    let r = ui.add(b);
    if enabled {
        r.on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        r
    }
}

/// Quiet tinted button for actions repeated on every row (Uninstall, Quit…).
fn soft(ui: &mut Ui, enabled: bool, text: impl Into<String>, color: Color32) -> Response {
    let (fill, fg) = if enabled { (color.gamma_multiply(0.15), color) } else { (Color32::from_rgb(42, 42, 47), Color32::from_rgb(118, 118, 126)) };
    let b = Button::new(RichText::new(text.into()).color(fg))
        .fill(fill)
        .stroke(Stroke::NONE)
        .rounding(8.0)
        .min_size(vec2(0.0, 30.0))
        .sense(if enabled { Sense::click() } else { Sense::hover() });
    let r = ui.add(b);
    if enabled {
        r.on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        r
    }
}

/// Small square icon button: dim at rest, a soft background on hover, red on hover when `danger`.
fn icon_btn(ui: &mut Ui, icon: &str, tip: &str, danger: bool) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(30.0, 30.0), Sense::click());
    paint_icon_btn(ui, rect, &resp, icon, danger);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(tip)
}

fn paint_icon_btn(ui: &Ui, rect: Rect, resp: &Response, icon: &str, danger: bool) {
    let hot = resp.hovered();
    let p = ui.painter();
    if hot {
        p.rect_filled(rect, 8.0, if danger { DANGER.gamma_multiply(0.18) } else { Color32::from_white_alpha(16) });
    }
    let fg = match (hot, danger) {
        (true, true) => DANGER,
        (true, false) => Color32::WHITE,
        _ => Color32::from_rgb(160, 160, 168),
    };
    p.text(rect.center(), Align2::CENTER_CENTER, icon, FontId::proportional(16.0), fg);
}

fn icon_button(ui: &mut Ui, icon: &str, color: Color32, tip: &str) -> Response {
    icon_btn(ui, icon, tip, color == DANGER)
}

/// Search box with an icon, a clear button and a focus ring. ⌘F focuses it, Esc clears it.
fn search_field(ui: &mut Ui, text: &mut String, hint: &str, width: f32) -> Response {
    let id = Id::new(("search", hint));
    if ui.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::F)) {
        ui.memory_mut(|m| m.request_focus(id));
    }
    let focused = ui.memory(|m| m.has_focus(id));
    Frame::none()
        .fill(Color32::from_rgb(36, 36, 40))
        .rounding(9.0)
        .stroke(Stroke::new(1.0_f32, if focused { ACCENT } else { BORDER }))
        .inner_margin(egui::Margin { left: 10.0, right: 4.0, top: 3.0, bottom: 3.0 })
        .show(ui, |ui| {
            ui.set_width(width);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                ui.label(RichText::new(ic::MAGNIFYING_GLASS).size(15.0).color(if focused { Color32::WHITE } else { DIM }));
                let clear_w = if text.is_empty() { 0.0 } else { 26.0 };
                let r = ui.add(
                    egui::TextEdit::singleline(text)
                        .id(id)
                        .hint_text(RichText::new(hint).color(Color32::from_rgb(120, 120, 128)))
                        .frame(false)
                        .margin(vec2(0.0, 5.0))
                        .desired_width(ui.available_width() - clear_w),
                );
                if r.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    text.clear();
                }
                if !text.is_empty() {
                    let (rect, resp) = ui.allocate_exact_size(vec2(22.0, 22.0), Sense::click());
                    let c = if resp.hovered() { Color32::WHITE } else { DIM };
                    ui.painter().text(rect.center(), Align2::CENTER_CENTER, ic::X_CIRCLE, FontId::proportional(15.0), c);
                    if resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text("Clear").clicked() {
                        text.clear();
                    }
                }
                r
            })
            .inner
        })
        .inner
}

/// Round selection mark used in lists.
fn paint_check(ui: &Ui, rect: Rect, on: bool, hot: bool) {
    let p = ui.painter();
    let c = rect.center();
    if on {
        p.circle_filled(c, 10.0, ACCENT);
        p.text(c, Align2::CENTER_CENTER, ic::CHECK, FontId::proportional(12.5), Color32::WHITE);
    } else {
        let s = if hot { Color32::from_rgb(150, 150, 158) } else { Color32::from_rgb(86, 86, 94) };
        p.circle_stroke(c, 9.0, Stroke::new(1.5_f32, s));
    }
}

/// Single-line text cut to `width` with an ellipsis.
fn truncated(ui: &Ui, text: &str, size: f32, color: Color32, width: f32) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::simple_singleline(text.to_string(), FontId::proportional(size), color);
    job.wrap = egui::text::TextWrapping::truncate_at_width(width.max(10.0));
    ui.painter().layout_job(job)
}

/// Replaces characters the bundled font can't draw (macOS puts a narrow no-break space in "12.09 AM").
fn disp(s: &str) -> String {
    s.replace(['\u{202f}', '\u{a0}'], " ")
}

fn header(ui: &mut Ui, icon: &str, color: Color32, title: &str, sub: &str, right: impl FnOnce(&mut Ui)) {
    ui.horizontal(|ui| {
        badge(ui, icon, color, 44.0);
        ui.add_space(4.0);
        ui.vertical(|ui| {
            ui.label(RichText::new(title).size(22.0).strong().color(Color32::WHITE));
            ui.label(RichText::new(sub).color(DIM));
        });
        ui.with_layout(Layout::right_to_left(Align::Center), right);
    });
    ui.add_space(16.0);
}

fn ring(ui: &mut Ui, frac: f32, color: Color32, size: f32, center_text: &str) {
    let (rect, _) = ui.allocate_exact_size(vec2(size, size), Sense::hover());
    let c = rect.center();
    let r = size / 2.0 - 6.0;
    let p = ui.painter();
    p.circle_stroke(c, r, Stroke::new(9.0_f32, TRACK));
    let n = 72;
    let pts: Vec<_> = (0..=n)
        .map(|i| {
            let a = -std::f32::consts::FRAC_PI_2 + std::f32::consts::TAU * frac.clamp(0.0, 1.0) * i as f32 / n as f32;
            c + vec2(a.cos(), a.sin()) * r
        })
        .collect();
    p.add(Shape::line(pts, Stroke::new(9.0_f32, color)));
    p.text(c, Align2::CENTER_CENTER, center_text, FontId::proportional(15.0), Color32::WHITE);
}

fn indeterminate(ui: &mut Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 5.0), Sense::hover());
    ui.painter().rect_filled(rect, 3.0, TRACK);
    let t = ui.input(|i| i.time) as f32;
    let seg = rect.width() * 0.28;
    let x = (t * 0.55).fract() * (rect.width() + seg) - seg;
    let bar = Rect::from_min_max(
        pos2((rect.left() + x).max(rect.left()), rect.top()),
        pos2((rect.left() + x + seg).min(rect.right()), rect.bottom()),
    );
    ui.painter().rect_filled(bar, 3.0, color);
    ui.ctx().request_repaint();
}

fn bar(ui: &mut Ui, frac: f32, color: Color32) {
    ui.add(ProgressBar::new(frac).desired_height(7.0).fill(color).rounding(4.0));
}

fn nav_item(ui: &mut Ui, selected: bool, icon: &str, color: Color32, text: &str, badge_text: Option<String>) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 34.0), Sense::click());
    let p = ui.painter();
    if selected {
        p.rect_filled(rect, 9.0, Color32::from_rgb(40, 40, 46));
        // Accent tick on the left edge.
        let tick = Rect::from_center_size(rect.left_center() + vec2(1.5, 0.0), vec2(3.0, 16.0));
        p.rect_filled(tick, 1.5, color);
    } else if resp.hovered() {
        p.rect_filled(rect, 9.0, Color32::from_rgb(29, 29, 33));
    }
    let fg = if selected { Color32::WHITE } else { Color32::from_rgb(196, 196, 204) };
    p.text(rect.left_center() + vec2(22.0, 0.0), Align2::CENTER_CENTER, icon, FontId::proportional(16.0), color);
    // Size on the right; the name is cut short so the two never overlap.
    let mut right = rect.right() - 10.0;
    if let Some(b) = badge_text {
        let g = p.layout_no_wrap(b, FontId::proportional(11.0), if selected { Color32::from_rgb(200, 200, 206) } else { DIM });
        right -= g.size().x;
        p.galley(pos2(right, rect.center().y - g.size().y / 2.0), g, DIM);
        right -= 10.0;
    }
    let left = rect.left() + 40.0;
    let g = truncated(ui, text, 13.5, fg, right - left);
    ui.painter().galley(pos2(left, rect.center().y - g.size().y / 2.0), g, fg);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn section(ui: &mut Ui, text: &str) {
    ui.add_space(14.0);
    ui.horizontal(|ui| {
        ui.add_space(10.0);
        ui.label(RichText::new(text).size(10.5).color(Color32::from_rgb(104, 104, 112)).strong());
    });
    ui.add_space(2.0);
}

fn short(p: &std::path::Path) -> String {
    disp(&match p.strip_prefix(scan::home()) {
        Ok(r) => format!("~/{}", r.display()),
        Err(_) => p.display().to_string(),
    })
}

/// Which identical copy stays: "Keeps “a.zip” here" or "Keeps the one in “Project”".
fn kept_note(copy: &std::path::Path, orig: &std::path::Path) -> String {
    let name = |p: &std::path::Path| p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let (cp, op) = (copy.parent().unwrap_or(copy), orig.parent().unwrap_or(orig));
    if cp == op {
        return format!("Keeps “{}” here", name(orig));
    }
    let a: Vec<_> = cp.iter().collect();
    let b: Vec<_> = op.iter().collect();
    let pre = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let suf = a[pre..].iter().rev().zip(b[pre..].iter().rev()).take_while(|(x, y)| x == y).count();
    let differs: Vec<_> = b[pre..b.len() - suf].iter().map(|c| c.to_string_lossy()).collect();
    if name(copy) == name(orig) && !differs.is_empty() && differs.len() <= 2 {
        format!("Keeps the one in “{}”", differs.join("/"))
    } else {
        format!("Keeps {}", short(orig))
    }
}

/// "today", "3 days ago", "5 months ago", "2 years ago".
fn ago(days: u64) -> String {
    match days {
        0 => "today".into(),
        1 => "yesterday".into(),
        2..=59 => format!("{days} days ago"),
        60..=729 => format!("{} months ago", days / 30),
        _ => format!("{} years ago", days / 365),
    }
}

fn idle_color(days: u64) -> Color32 {
    match days {
        365.. => DANGER,
        90.. => WARN,
        _ => DIM,
    }
}

#[derive(Default)]
struct RowOut {
    reveal: bool,
    delete: bool,
    open: bool,
    look: bool,
    /// The row itself (not a button) was clicked.
    clicked: bool,
}

const ROW_H: f32 = 60.0;

/// One file/folder row: selection mark, preview, name and path, last use, size and actions.
fn item_row(ui: &mut Ui, it: &mut Item, busy: bool, show_check: bool, thumbs: &mut thumbs::Thumbs) -> RowOut {
    let mut out = RowOut::default();
    let (rect, row) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::click());
    if !ui.is_rect_visible(rect) {
        return out;
    }
    let hot = ui.rect_contains_pointer(rect) && !busy;
    let bg = if it.selected && show_check {
        Some(ACCENT.gamma_multiply(0.16))
    } else if hot {
        Some(Color32::from_white_alpha(7))
    } else {
        None
    };
    if let Some(f) = bg {
        ui.painter().rect_filled(rect, 10.0, f);
    }
    if show_check && !busy {
        out.clicked = row.clicked();
    }
    if row.double_clicked() {
        out.open = true;
    }
    let cy = rect.center().y;
    let mut x = rect.left() + 8.0;
    if show_check {
        paint_check(ui, Rect::from_center_size(pos2(x + 10.0, cy), vec2(20.0, 20.0)), it.selected, hot);
        x += 30.0;
    }

    // Preview (thumbnail, app icon or category icon). Clicking it opens Quick Look.
    let thumb = 42.0;
    let mut tui = ui.new_child(egui::UiBuilder::new().max_rect(Rect::from_min_size(pos2(x, cy - thumb / 2.0), vec2(thumb, thumb))));
    let tr = thumbs::preview(&mut tui, thumbs, &it.path, thumb, cat_icon(it.cat), cat_color(it.cat));
    if thumbs::previewable(&it.path) && !thumbs::is_app(&it.path) && tr.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
        out.look = true;
    }
    x += thumb + 12.0;

    // Right side: actions, size, last used.
    let mut right = rect.right() - 6.0;
    if busy {
        let r = Rect::from_min_max(pos2(right - 110.0, rect.top()), pos2(right, rect.bottom()));
        let mut bui = ui.new_child(egui::UiBuilder::new().max_rect(r).layout(Layout::right_to_left(Align::Center)));
        bui.add(egui::Spinner::new().size(16.0).color(DANGER));
        bui.label(RichText::new("Deleting…").small().color(DIM));
        right -= 116.0;
    } else {
        let file = it.path.is_file() || thumbs::is_app(&it.path);
        let mut btns: Vec<(&str, &str, bool, u8)> = vec![(ic::TRASH, "Delete", true, 0), (ic::FOLDER_OPEN, "Show in Finder", false, 1)];
        if file {
            btns.push((ic::ARROW_SQUARE_OUT, "Open", false, 2));
            if !thumbs::is_app(&it.path) {
                btns.push((ic::EYE, "Quick Look (Space)", false, 3));
            }
        }
        for (icon, tip, danger, k) in btns {
            let r = Rect::from_center_size(pos2(right - 15.0, cy), vec2(30.0, 30.0));
            let resp = ui.interact(r, Id::new(("row-btn", &it.path, k)), Sense::click());
            paint_icon_btn(ui, r, &resp, icon, danger);
            if resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(tip).clicked() {
                match k {
                    0 => out.delete = true,
                    1 => out.reveal = true,
                    2 => out.open = true,
                    _ => out.look = true,
                }
            }
            right -= 32.0;
        }
        right -= 8.0;
    }
    let p = ui.painter();
    p.text(pos2(right, cy), Align2::RIGHT_CENTER, human(it.size), FontId::proportional(14.5), Color32::WHITE);
    right -= 92.0;
    if let Some(d) = it.idle_days() {
        let r = Rect::from_min_max(pos2(right - 104.0, cy - 10.0), pos2(right, cy + 10.0));
        p.text(r.right_center(), Align2::RIGHT_CENTER, format!("{} {}", ic::CLOCK, ago(d)), FontId::proportional(12.0), idle_color(d));
        let tip = if it.cat == Cat::Dev { "Last change in this project" } else { "Last opened or modified" };
        ui.interact(r, Id::new(("idle", &it.path)), Sense::hover()).on_hover_text(tip);
        right -= 112.0;
    }

    // Name and path.
    let w = right - x - 8.0;
    let title = truncated(ui, &disp(&it.label), 14.0, Color32::WHITE, w);
    ui.painter().galley(pos2(x, cy - title.size().y - 1.0), title, Color32::WHITE);
    if let Some(orig) = &it.dup_of {
        // "~/Downloads/Project copy/…   ✓ Keeps the one in “Project”"
        let keep = truncated(ui, &format!("{}  {}", ic::SHIELD_CHECK, disp(&kept_note(&it.path, orig))), 11.5, SUCCESS, w * 0.5);
        let gap = 14.0;
        let folder = it.path.parent().unwrap_or(&it.path);
        let sub = truncated(ui, &short(folder), 11.5, DIM, w - keep.size().x - gap);
        let kx = x + sub.size().x + gap;
        let r = Rect::from_min_size(pos2(x, cy + 2.0), vec2(kx + keep.size().x - x, keep.size().y));
        ui.painter().galley(pos2(x, cy + 2.0), sub, DIM);
        ui.painter().galley(pos2(kx, cy + 2.0), keep, SUCCESS);
        ui.interact(r, Id::new(("dup", &it.path)), Sense::hover())
            .on_hover_text(format!("This copy:  {}\nKept copy:  {}", short(&it.path), short(orig)));
    } else {
        let sub = truncated(ui, &short(&it.path), 11.5, DIM, w);
        ui.painter().galley(pos2(x, cy + 2.0), sub, DIM);
    }
    out
}

/// Handles the Open / Show in Finder / Quick Look buttons of a row.
fn row_actions(r: &RowOut, path: &std::path::Path) {
    if r.reveal {
        system::reveal(path);
    }
    if r.open {
        system::open_path(path);
    }
    if r.look {
        system::quick_look(path);
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        self.handle_messages();
        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect());
        if !dropped.is_empty() {
            // Folders bring their photos; photos go to the photo tab, everything else to videos.
            let mut pics: Vec<PathBuf> = dropped.iter().filter(|p| p.is_dir()).flat_map(|d| photos::collect(d)).collect();
            pics.extend(dropped.iter().filter(|p| photos::is_photo(p)).cloned());
            let vids: Vec<PathBuf> = dropped.into_iter().filter(|p| p.is_file() && !photos::is_photo(p)).collect();
            self.photos_tab = !pics.is_empty() && vids.is_empty();
            self.add_photos(pics);
            self.add_videos(vids);
            self.page = Page::Video;
        }
        self.thumbs.poll();
        if matches!(self.page, Page::Dashboard | Page::Speed) {
            self.sampler.want();
            if let Some(m) = self.sampler.live.lock().unwrap().mem {
                self.mem = Some(m);
            }
            if self.last_sys.map_or(true, |t| t.elapsed() > Duration::from_secs(3)) {
                self.disk = system::disk();
                if self.page == Page::Speed {
                    self.awake = system::awake_until();
                }
                self.last_sys = Some(Instant::now());
            }
        }
        if self.last_batt.map_or(true, |t| t.elapsed() > Duration::from_secs(5)) {
            self.batt = battery::info();
            if matches!(self.page, Page::Battery | Page::Alerts) {
                self.helper = battery::helper_status();
                self.agent_on = agent::agent_installed();
            }
            self.last_batt = Some(Instant::now());
        }
        if self.scanning || self.del_total > 0 || *self.encoding.lock().unwrap() || *self.photo_busy.lock().unwrap() {
            ctx.request_repaint_after(Duration::from_millis(120));
        } else {
            ctx.request_repaint_after(Duration::from_secs(3));
        }

        if self.onboarding.is_some() {
            self.onboarding(ctx);
            return;
        }
        self.sidebar(ctx);
        egui::CentralPanel::default()
            .frame(Frame::none().fill(BG).inner_margin(egui::Margin::symmetric(28.0, 22.0)))
            .show(ctx, |ui| match self.page {
                Page::Dashboard => self.dashboard(ui),
                Page::Find => self.find_page(ui),
                Page::Review => self.review_page(ui),
                Page::Cat(cat) => self.category(ui, cat),
                Page::Video => self.video_page(ui, ctx),
                Page::Speed => self.speed_page(ui),
                Page::Battery => self.battery_page(ui),
                Page::Alerts => self.alerts_page(ui),
                Page::DiskMap => self.diskmap_page(ui),
                Page::Uninstaller => self.uninstall_page(ui),
                Page::Privacy => self.privacy_page(ui),
                Page::Startup => self.startup_page(ui),
            });
        self.overlays(ctx);
    }
}

impl App {
    fn sidebar(&mut self, ctx: &egui::Context) {
        egui::SidePanel::left("nav")
            .resizable(false)
            .exact_width(236.0)
            .frame(Frame::none().fill(SIDEBAR).inner_margin(egui::Margin::symmetric(14.0, 18.0)))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    badge(ui, ic::SPARKLE, ACCENT, 34.0);
                    ui.label(RichText::new("Clean You").size(19.0).strong().color(Color32::WHITE));
                });
                ui.add_space(14.0);
                if let Some((free, total)) = self.disk {
                    Frame::none().fill(CARD).rounding(12.0).stroke(Stroke::new(1.0_f32, BORDER)).inner_margin(12.0).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        let used = 1.0 - free as f32 / total as f32;
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(ic::HARD_DRIVE).color(DIM));
                            ui.label(RichText::new("Macintosh HD").size(12.5).strong().color(Color32::WHITE));
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                ui.label(RichText::new(format!("{:.0}%", used * 100.0)).size(11.5).color(DIM));
                            });
                        });
                        bar(ui, used, if used > 0.9 { DANGER } else { ACCENT });
                        ui.label(RichText::new(format!("{} free of {}", human(free), human(total))).size(11.5).color(DIM));
                        if let Some(b) = &self.batt {
                            ui.add_space(4.0);
                            let (icon, c) = if b.external {
                                (ic::BATTERY_CHARGING, SUCCESS)
                            } else if b.pct <= 20 {
                                (ic::BATTERY_WARNING, DANGER)
                            } else {
                                (ic::BATTERY_HIGH, DIM)
                            };
                            ui.label(RichText::new(format!("{icon} {}%  ·  health {:.0}%", b.pct, b.health * 100.0)).size(11.5).color(c));
                        }
                    });
                }

                let mut go = None;
                let violet = Color32::from_rgb(94, 92, 230);
                let yellow = Color32::from_rgb(255, 214, 10);
                let pink = Color32::from_rgb(255, 55, 95);
                ui.add_space(4.0);
                egui::ScrollArea::vertical()
                    .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::VisibleWhenNeeded)
                    .auto_shrink([false, true])
                    .max_height(ui.available_height() - 28.0)
                    .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    let mut nav = |ui: &mut Ui, page: Page, icon: &str, color: Color32, text: &str, badge: Option<String>| {
                        if nav_item(ui, self.page == page, icon, color, text, badge).clicked() {
                            go = Some(page);
                        }
                    };
                    section(ui, "OVERVIEW");
                    nav(ui, Page::Dashboard, ic::SQUARES_FOUR, ACCENT, "Dashboard", None);
                    nav(ui, Page::DiskMap, ic::CHART_DONUT, Color32::from_rgb(100, 210, 255), "Disk Map", None);
                    nav(ui, Page::Find, ic::MAGNIFYING_GLASS, Color32::from_rgb(102, 212, 207), "Find", None);
                    let staged = review::total(&self.staged);
                    nav(ui, Page::Review, ic::LIST_CHECKS, SUCCESS, "Cleanup List", (!self.staged.is_empty()).then(|| human(staged)));
                    section(ui, "CLEANUP");
                    for cat in Cat::ALL {
                        if cat == Cat::Recordings {
                            continue;
                        }
                        let (n, size) = self.total(cat);
                        nav(ui, Page::Cat(cat), cat_icon(cat), cat_color(cat), cat.title(), (n > 0).then(|| human(size)));
                    }
                    nav(ui, Page::Uninstaller, ic::PACKAGE, violet, "App Uninstaller", None);
                    nav(ui, Page::Privacy, ic::EYE_SLASH, pink, "Privacy", None);
                    section(ui, "MEDIA");
                    let (n, size) = self.total(Cat::Recordings);
                    nav(ui, Page::Cat(Cat::Recordings), cat_icon(Cat::Recordings), cat_color(Cat::Recordings), Cat::Recordings.title(), (n > 0).then(|| human(size)));
                    nav(ui, Page::Video, ic::FILM_STRIP, violet, "Compress", None);
                    section(ui, "OPTIMIZE");
                    nav(ui, Page::Speed, ic::ROCKET_LAUNCH, yellow, "Performance", None);
                    nav(ui, Page::Startup, ic::POWER, WARN, "Startup Items", None);
                    nav(ui, Page::Battery, ic::BATTERY_CHARGING, SUCCESS, "Battery Care", None);
                    nav(ui, Page::Alerts, ic::BELL, pink, "Smart Alerts", None);
                });
                if let Some(p) = go {
                    self.page = p;
                    self.query.clear();
                }

                ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
                    self.version_row(ui);
                    if self.scanning {
                        ui.horizontal(|ui| {
                            ui.add(egui::Spinner::new().size(14.0).color(ACCENT));
                            ui.label(RichText::new("Scanning…").small().color(DIM));
                        });
                    }
                });
            });
    }

    fn overlays(&mut self, ctx: &egui::Context) {
        // Delete confirmation modal
        if self.confirm.is_some() {
            egui::Area::new(Id::new("dim")).order(Order::Middle).fixed_pos(pos2(0.0, 0.0)).show(ctx, |ui| {
                let screen = ctx.screen_rect();
                ui.painter().rect_filled(screen, 0.0, Color32::from_black_alpha(170));
                ui.allocate_rect(screen, Sense::click());
            });
            let (mut cancel, mut go) = (ctx.input(|i| i.key_pressed(egui::Key::Escape)), false);
            let c = self.confirm.as_mut().unwrap();
            egui::Area::new(Id::new("confirm")).order(Order::Foreground).anchor(Align2::CENTER_CENTER, [0.0, 0.0]).show(
                ctx,
                |ui| {
                    Frame::none().fill(CARD).rounding(16.0).stroke(Stroke::new(1.0_f32, BORDER)).inner_margin(24.0).show(ui, |ui| {
                        ui.set_width(400.0);
                        badge(ui, ic::TRASH, DANGER, 44.0);
                        ui.add_space(8.0);
                        ui.label(RichText::new(&c.title).size(18.0).strong().color(Color32::WHITE));
                        ui.label(RichText::new(format!("{} item(s) · {}", c.entries.len(), human(c.size))).color(DIM));
                        ui.add_space(8.0);
                        for l in c.labels.iter().take(4) {
                            ui.add(Label::new(RichText::new(format!("•  {l}")).small().color(DIM)).truncate());
                        }
                        if c.labels.len() > 4 {
                            ui.label(RichText::new(format!("   and {} more", c.labels.len() - 4)).small().color(DIM));
                        }
                        if c.title.contains("uplicate") {
                            ui.add_space(6.0);
                            ui.label(RichText::new(format!("{}  One copy of every file stays where it is.", ic::SHIELD_CHECK)).small().color(SUCCESS));
                        }
                        ui.add_space(12.0);
                        ui.checkbox(&mut c.permanent, "Delete permanently (frees space right away)");
                        let note = if c.permanent {
                            RichText::new(format!("{} This can't be undone.", ic::WARNING)).color(WARN).small()
                        } else {
                            RichText::new("Moves to Trash. Space is freed when you empty the Trash.").color(DIM).small()
                        };
                        ui.label(note);
                        ui.add_space(16.0);
                        ui.horizontal(|ui| {
                            let label = if c.permanent {
                                format!("{} Delete {}", ic::TRASH, human(c.size))
                            } else {
                                format!("{} Move to Trash", ic::TRASH)
                            };
                            go = primary(ui, true, label, DANGER).clicked();
                            if ui.add(Button::new("Cancel").rounding(9.0).min_size(vec2(80.0, 34.0))).clicked() {
                                cancel = true;
                            }
                        });
                    });
                },
            );
            if go {
                let c = self.confirm.take().unwrap();
                self.start_delete(c.entries, c.permanent);
            } else if cancel {
                self.confirm = None;
            }
        }

        // Progress / result toast
        let content: Option<(String, Color32, &str, bool)> = if self.del_total > 0 {
            let n = (self.del_done + 1).min(self.del_total);
            Some((format!("Deleting {n} of {} · freed {}", self.del_total, human(self.freed)), DANGER, "", true))
        } else if let Some(t) = self.toast.as_ref().filter(|t| t.at.elapsed() < Duration::from_secs(6)) {
            let (c, i) = if t.ok { (SUCCESS, ic::CHECK_CIRCLE) } else { (WARN, ic::WARNING) };
            Some((t.text.clone(), c, i, false))
        } else {
            None
        };
        if let Some((text, color, icon, spin)) = content {
            egui::Area::new(Id::new("toast"))
                .order(Order::Foreground)
                .anchor(Align2::CENTER_BOTTOM, [118.0, -24.0])
                .interactable(false)
                .show(ctx, |ui| {
                    Frame::none()
                        .fill(Color32::from_rgb(40, 40, 45))
                        .rounding(22.0)
                        .stroke(Stroke::new(1.0_f32, BORDER))
                        .inner_margin(egui::Margin::symmetric(18.0, 11.0))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                if spin {
                                    ui.add(egui::Spinner::new().size(16.0).color(color));
                                } else {
                                    ui.label(RichText::new(icon).size(17.0).color(color));
                                }
                                ui.label(RichText::new(text).color(Color32::WHITE));
                            });
                        });
                });
        }
    }

    fn scan_loader(&mut self, ui: &mut Ui) {
        card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(26.0).color(ACCENT));
                ui.add_space(6.0);
                ui.vertical(|ui| {
                    ui.label(RichText::new("Scanning your Mac…").size(16.0).strong().color(Color32::WHITE));
                    ui.add(Label::new(RichText::new(&self.scan_path).small().color(DIM)).truncate());
                });
            });
            ui.add_space(12.0);
            indeterminate(ui, ACCENT);
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                for p in PHASES {
                    let done = self.phases_done.contains(&p);
                    let (i, c) = if done { (ic::CHECK_CIRCLE, SUCCESS) } else { (ic::CIRCLE_NOTCH, DIM) };
                    ui.label(RichText::new(format!("{i}  {p}")).color(c));
                    ui.add_space(12.0);
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let secs = self.scan_started.elapsed().as_secs();
                    ui.label(RichText::new(format!("{}:{:02}", secs / 60, secs % 60)).monospace().color(DIM));
                    ui.label(RichText::new(format!("{} files · {} found", self.scan_files, self.items.len())).color(DIM));
                });
            });
        });
        ui.add_space(14.0);
    }

    fn dashboard(&mut self, ui: &mut Ui) {
        let scanning = self.scanning;
        let mut scan = false;
        header(ui, ic::SQUARES_FOUR, ACCENT, "Dashboard", "Everything taking space on your Mac, in one place.", |ui| {
            if self.scanned || scanning {
                let t = if scanning { "Scanning…" } else { "Scan again" };
                scan = primary(ui, !scanning, format!("{}  {t}", ic::ARROWS_CLOCKWISE), ACCENT).clicked();
            }
        });

        egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
            if !self.scanned && !scanning {
                card(ui, |ui| {
                    ui.add_space(30.0);
                    ui.vertical_centered(|ui| {
                        badge(ui, ic::MAGNIFYING_GLASS, ACCENT, 72.0);
                        ui.add_space(12.0);
                        ui.label(RichText::new("Let's find what's filling your Mac").size(24.0).strong().color(Color32::WHITE));
                        ui.label(RichText::new("Caches, developer junk, AI models, games, big files and recordings.").color(DIM));
                        ui.add_space(18.0);
                        let b = Button::new(
                            RichText::new(format!("{}  Start scan", ic::MAGNIFYING_GLASS)).size(16.0).strong().color(Color32::WHITE),
                        )
                        .fill(ACCENT)
                        .rounding(22.0)
                        .min_size(vec2(190.0, 44.0));
                        scan |= ui.add(b).clicked();
                        ui.add_space(14.0);
                        if ui.link(format!("{} Give Full Disk Access for a complete scan", ic::SHIELD_CHECK)).clicked() {
                            system::open("x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles");
                        }
                    });
                    ui.add_space(30.0);
                });
                return;
            }
            if scanning {
                self.scan_loader(ui);
            }

            // ----- stat cards -----
            let reclaim = self.reclaimable();
            let safe_idx: Vec<usize> = (0..self.items.len()).filter(|i| self.items[*i].safe).collect();
            let safe_size: u64 = safe_idx.iter().map(|i| self.items[*i].size).sum();
            let busy = self.del_total > 0;
            let (disk, mem) = (self.disk, self.mem);
            let (mut smart, mut go_speed) = (false, false);
            ui.columns(3, |cols| {
                card(&mut cols[0], |ui| {
                    ui.set_min_height(150.0);
                    ui.label(RichText::new(format!("{}  Ready to clean", ic::SPARKLE)).color(DIM));
                    ui.label(RichText::new(human(reclaim)).size(34.0).strong().color(Color32::WHITE));
                    ui.add_space(6.0);
                    smart = primary(ui, safe_size > 0 && !busy, format!("{}  Smart Clean · {}", ic::BROOM, human(safe_size)), ACCENT)
                        .on_hover_text("Removes caches, logs and build data only. Apps rebuild them as needed.")
                        .clicked();
                });
                card(&mut cols[1], |ui| {
                    ui.set_min_height(150.0);
                    ui.label(RichText::new(format!("{}  Storage", ic::HARD_DRIVE)).color(DIM));
                    if let Some((free, total)) = disk {
                        ui.horizontal(|ui| {
                            let used = 1.0 - free as f32 / total as f32;
                            ring(ui, used, if used > 0.9 { DANGER } else { ACCENT }, 96.0, &format!("{:.0}%", used * 100.0));
                            ui.vertical(|ui| {
                                ui.add_space(14.0);
                                ui.label(RichText::new(human(free)).size(20.0).strong().color(Color32::WHITE));
                                ui.label(RichText::new(format!("free of {}", human(total))).small().color(DIM));
                                ui.label(RichText::new(format!("+{} possible", human(reclaim))).small().color(SUCCESS));
                            });
                        });
                    }
                });
                card(&mut cols[2], |ui| {
                    ui.set_min_height(150.0);
                    ui.label(RichText::new(format!("{}  Memory", ic::MEMORY)).color(DIM));
                    if let Some((used, total)) = mem {
                        ui.horizontal(|ui| {
                            let f = used as f32 / total as f32;
                            ring(ui, f, if f > 0.85 { DANGER } else { SUCCESS }, 96.0, &format!("{:.0}%", f * 100.0));
                            ui.vertical(|ui| {
                                ui.add_space(14.0);
                                ui.label(RichText::new(human(used)).size(20.0).strong().color(Color32::WHITE));
                                ui.label(RichText::new(format!("used of {}", human(total))).small().color(DIM));
                                go_speed = ui.link(format!("Boost {}", ic::ARROW_RIGHT)).clicked();
                            });
                        });
                    }
                });
            });
            if smart {
                self.ask_delete("Smart Clean", safe_idx, true);
            }
            if go_speed {
                self.page = Page::Speed;
            }

            ui.add_space(14.0);
            self.overview_card(ui);

            // ----- category cards -----
            ui.add_space(14.0);
            self.activity_card(ui);

            ui.add_space(22.0);
            ui.label(RichText::new("Categories").size(16.0).strong().color(Color32::WHITE));
            ui.add_space(8.0);
            let totals: Vec<(Cat, usize, u64)> = Cat::ALL
                .iter()
                .map(|c| {
                    let (n, s) = self.total(*c);
                    (*c, n, s)
                })
                .collect();
            let mut open = None;
            for chunk in totals.chunks(3) {
                ui.columns(3, |cols| {
                    for (col, (cat, n, size)) in cols.iter_mut().zip(chunk) {
                        let r = card(col, |ui| {
                            ui.horizontal(|ui| {
                                badge(ui, cat_icon(*cat), cat_color(*cat), 38.0);
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    ui.label(RichText::new(ic::CARET_RIGHT).color(DIM));
                                });
                            });
                            ui.add_space(8.0);
                            ui.label(RichText::new(cat.title()).strong().color(Color32::WHITE));
                            ui.label(RichText::new(human(*size)).size(22.0).strong().color(cat_color(*cat)));
                            ui.label(RichText::new(format!("{n} items")).small().color(DIM));
                        });
                        let resp = col.interact(r.response.rect, Id::new(("cat-card", *cat)), Sense::click());
                        if resp.hovered() {
                            col.painter().rect_stroke(r.response.rect, 14.0, Stroke::new(1.5_f32, cat_color(*cat)));
                        }
                        if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                            open = Some(*cat);
                        }
                    }
                });
                ui.add_space(10.0);
            }
            if let Some(c) = open {
                self.page = Page::Cat(c);
            }

            // ----- biggest items -----
            ui.add_space(12.0);
            ui.label(RichText::new("Biggest items").size(16.0).strong().color(Color32::WHITE));
            ui.add_space(8.0);
            let mut idx: Vec<usize> = (0..self.items.len()).filter(|i| self.items[*i].cat != Cat::Recordings).collect();
            idx.sort_by(|a, b| self.items[*b].size.cmp(&self.items[*a].size));
            idx.truncate(8);
            let mut action = None;
            card(ui, |ui| {
                if idx.is_empty() {
                    ui.label(RichText::new("Nothing yet").color(DIM));
                }
                for i in idx.iter() {
                    let busy = self.deleting.contains(&self.items[*i].path);
                    let r = item_row(ui, &mut self.items[*i], busy, false, &mut self.thumbs);
                    row_actions(&r, &self.items[*i].path);
                    if r.delete {
                        action = Some(*i);
                    }
                }
            });
            if let Some(i) = action {
                let cat = self.items[i].cat;
                self.ask_delete(format!("Delete {}?", self.items[i].label), vec![i], cat.regenerable());
            }
        });
        if scan {
            self.start_scan();
        }
    }

    fn category(&mut self, ui: &mut Ui, cat: Cat) {
        let (_, total) = self.total(cat);
        header(ui, cat_icon(cat), cat_color(cat), cat.title(), cat.hint(), |ui| {
            ui.label(RichText::new(human(total)).size(22.0).strong().color(cat_color(cat)));
        });

        if !self.scanned && !self.scanning {
            card(ui, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(20.0);
                    ui.label(RichText::new("Run a scan to see what's here").color(DIM));
                    ui.add_space(10.0);
                    if primary(ui, true, format!("{}  Start scan", ic::MAGNIFYING_GLASS), ACCENT).clicked() {
                        self.start_scan();
                    }
                    ui.add_space(20.0);
                });
            });
            return;
        }
        if self.scanning {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(14.0).color(ACCENT));
                ui.add(
                    Label::new(RichText::new(format!("Still scanning, new items appear as they're found · {}", self.scan_path)).small().color(DIM))
                        .truncate(),
                );
            });
            indeterminate(ui, cat_color(cat));
            ui.add_space(10.0);
        }

        // Toolbar
        let q = self.query.to_lowercase();
        let min = self.min_large();
        let base: Vec<usize> = (0..self.items.len()).filter(|i| matches(&self.items[*i], cat, min, &q)).collect();
        let idle = self.min_idle;
        let ids: Vec<usize> =
            base.iter().copied().filter(|i| idle == 0 || self.items[*i].idle_days().is_some_and(|d| d >= idle)).collect();
        let sel: Vec<usize> =
            ids.iter().copied().filter(|i| self.items[*i].selected && !self.deleting.contains(&self.items[*i].path)).collect();
        let sel_size: u64 = sel.iter().map(|i| self.items[*i].size).sum();
        let all = !ids.is_empty() && ids.iter().all(|i| self.items[*i].selected);
        let (mut remove, mut remove_all, mut stage) = (false, false, false);
        ui.horizontal(|ui| {
            search_field(ui, &mut self.query, "Search name or path", 240.0);
            let t = if all { format!("{}  Select none", ic::SQUARE) } else { format!("{}  Select all", ic::CHECK_SQUARE) };
            if soft(ui, !ids.is_empty(), t, Color32::from_rgb(210, 210, 218)).clicked() {
                for i in &ids {
                    self.items[*i].selected = !all;
                }
            }
            if cat == Cat::Large {
                ui.label(RichText::new(ic::FUNNEL).color(DIM));
                ui.add(egui::Slider::new(&mut self.min_large_mb, 100..=10_000).logarithmic(true).suffix(" MB"));
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if cat == Cat::Duplicates && sel.is_empty() {
                    let size: u64 = ids.iter().map(|i| self.items[*i].size).sum();
                    let t = format!("{}  Remove all {} duplicates · {}", ic::BROOM, ids.len(), human(size));
                    remove_all = primary(ui, !ids.is_empty() && self.del_total == 0 && !self.scanning, t, DANGER)
                        .on_hover_text("Moves every extra copy to the Trash. One copy of each file is always kept.")
                        .clicked();
                    return;
                }
                let t = if sel.is_empty() {
                    format!("{}  Select items to delete", ic::TRASH)
                } else {
                    format!("{}  Delete {} · {}", ic::TRASH, sel.len(), human(sel_size))
                };
                remove = primary(ui, !sel.is_empty() && self.del_total == 0, t, DANGER).clicked();
                if !sel.is_empty() {
                    stage = soft(ui, true, format!("{}  Add to Cleanup list", ic::LIST_PLUS), SUCCESS)
                        .on_hover_text("Set these aside, review them with everything else, then move them to the Trash together")
                        .clicked();
                }
            });
        });
        // Not used in N days: one chip per bucket with its count and size; click to filter.
        ui.add_space(8.0);
        ui.horizontal_wrapped(|ui| {
            let what = if cat == Cat::Dev { "Projects untouched for" } else { "Not used for" };
            ui.label(RichText::new(format!("{}  {what}", ic::CLOCK_COUNTDOWN)).color(DIM));
            for (days, name) in [(0, "Any time"), (30, "30+ days"), (90, "90+ days"), (180, "6+ months"), (365, "1+ year")] {
                let (n, size) = base
                    .iter()
                    .filter(|i| days == 0 || self.items[**i].idle_days().is_some_and(|d| d >= days))
                    .fold((0, 0), |(n, s), i| (n + 1, s + self.items[*i].size));
                let on = self.min_idle == days;
                let color = if days == 0 { Color32::WHITE } else { idle_color(days) };
                let text = RichText::new(format!("{name} · {n} · {}", human(size))).color(if on { Color32::WHITE } else { color });
                let b = Button::new(text).rounding(14.0).fill(if on { ACCENT } else { Color32::from_rgb(44, 44, 49) });
                if ui.add_enabled(n > 0 || on, b).clicked() {
                    self.min_idle = days;
                }
            }
        });
        if cat == Cat::Recordings {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui
                    .button(format!("{}  Move all to one folder", ic::FOLDER_SIMPLE_PLUS))
                    .on_hover_text("~/Movies/Screen Recordings")
                    .clicked()
                {
                    self.gather_recordings();
                }
                if ui.button(format!("{}  Compress selected", ic::FILM_STRIP)).clicked() {
                    let paths: Vec<PathBuf> = sel.iter().map(|i| self.items[*i].path.clone()).collect();
                    if paths.is_empty() {
                        self.toast("Select some recordings first", false);
                    } else {
                        self.add_videos(paths);
                        self.page = Page::Video;
                    }
                }
                if ui
                    .button(format!("{}  Save future recordings there", ic::GEAR))
                    .on_hover_text("Changes where macOS saves new screenshots and screen recordings")
                    .clicked()
                {
                    let dest = scan::recordings_dir();
                    let _ = std::fs::create_dir_all(&dest);
                    let _ = std::process::Command::new("/usr/bin/defaults")
                        .args(["write", "com.apple.screencapture", "location"])
                        .arg(&dest)
                        .status();
                    let _ = std::process::Command::new("/usr/bin/killall").arg("SystemUIServer").status();
                    self.toast("New screenshots & recordings will save to Movies › Screen Recordings", true);
                }
            });
        }
        ui.add_space(4.0);
        ui.label(RichText::new("Click a row to select it · ⌘-click to add or remove · Shift-click to select a range · Space to Quick Look").small().color(DIM));
        ui.add_space(8.0);
        if remove {
            self.ask_delete(format!("Delete from {}?", cat.title()), sel.clone(), cat.regenerable());
        }
        if remove_all {
            self.ask_delete("Remove all duplicate copies?".to_string(), ids.clone(), false);
        }
        if stage {
            let items = sel.iter().map(|i| (self.items[*i].path.clone(), self.items[*i].label.clone(), self.items[*i].size)).collect();
            self.stage(items);
            for i in &sel {
                self.items[*i].selected = false;
            }
        }

        let mut single = None;
        let mut clicked = None;
        Frame::none().fill(CARD).rounding(14.0).stroke(Stroke::new(1.0_f32, BORDER)).inner_margin(8.0).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 2.0;
            egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
                if ids.is_empty() {
                    ui.add_space(40.0);
                    ui.vertical_centered(|ui| {
                        if self.scanning {
                            ui.label(RichText::new("Looking…").color(DIM));
                        } else {
                            ui.label(RichText::new(ic::CHECK_CIRCLE).size(40.0).color(SUCCESS));
                            ui.label(RichText::new("All clean here").color(DIM));
                        }
                    });
                }
                for (k, i) in ids.iter().enumerate() {
                    let busy = self.deleting.contains(&self.items[*i].path);
                    let r = item_row(ui, &mut self.items[*i], busy, true, &mut self.thumbs);
                    row_actions(&r, &self.items[*i].path);
                    if r.delete {
                        single = Some(*i);
                    }
                    if r.clicked {
                        clicked = Some(k);
                    }
                }
            });
        });
        if let Some(k) = clicked {
            self.click_row(ui, &ids, k);
        }
        // Space previews the last clicked row, like Finder.
        let typing = ui.ctx().wants_keyboard_input();
        if !typing && ui.input(|i| i.key_pressed(egui::Key::Space)) {
            if let Some(a) = self.anchor.filter(|a| ids.contains(a)) {
                system::quick_look(&self.items[a].path);
            }
        }
        if let Some(i) = single {
            self.ask_delete(format!("Delete {}?", self.items[i].label), vec![i], cat.regenerable());
        }
    }

    /// Finder-style selection: click picks one row, ⌘-click toggles, ⇧-click selects a range.
    fn click_row(&mut self, ui: &Ui, ids: &[usize], k: usize) {
        let m = ui.input(|i| i.modifiers);
        let i = ids[k];
        let anchor = self.anchor.and_then(|a| ids.iter().position(|x| *x == a));
        if m.shift && anchor.is_some() {
            let a = anchor.unwrap();
            let (lo, hi) = (a.min(k), a.max(k));
            if !m.command {
                for x in ids {
                    self.items[*x].selected = false;
                }
            }
            for x in &ids[lo..=hi] {
                self.items[*x].selected = true;
            }
            return; // keep the anchor so the range can be grown or shrunk
        }
        if m.command {
            self.items[i].selected = !self.items[i].selected;
        } else {
            let only = self.items[i].selected && ids.iter().filter(|x| self.items[**x].selected).count() == 1;
            for x in ids {
                self.items[*x].selected = false;
            }
            self.items[i].selected = !only;
        }
        self.anchor = Some(i);
    }

    fn video_page(&mut self, ui: &mut Ui, ctx: &egui::Context) {
        let violet = Color32::from_rgb(94, 92, 230);
        let sub = if self.photos_tab {
            "Shrinks photos to HEIC or JPEG instead of deleting them. Originals stay unless you choose otherwise."
        } else {
            "Shrinks videos to HEVC at the same resolution, and can restore them to their original size later."
        };
        let mut tab = self.photos_tab;
        header(ui, if tab { ic::IMAGE } else { ic::FILM_STRIP }, violet, "Compress", sub, |ui| {
            if chip(ui, tab, &format!("{}  Photos", ic::IMAGE)) {
                tab = true;
            }
            if chip(ui, !tab, &format!("{}  Videos", ic::FILM_STRIP)) {
                tab = false;
            }
        });
        self.photos_tab = tab;
        if tab {
            self.photos_panel(ui, ctx);
            return;
        }

        if video::tool("ffmpeg").is_none() {
            card(ui, |ui| {
                ui.label(RichText::new(format!("{} ffmpeg is required. Install it with:  brew install ffmpeg", ic::WARNING)).color(WARN));
            });
            return;
        }
        let busy = *self.encoding.lock().unwrap();

        // Drop zone
        let r = card(ui, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(6.0);
                ui.label(RichText::new(ic::FILE_ARROW_DOWN).size(34.0).color(violet));
                ui.label(RichText::new("Drop videos here").size(16.0).strong().color(Color32::WHITE));
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.add_space((ui.available_width() - 330.0).max(0.0) / 2.0);
                    if ui.button(format!("{}  Choose videos", ic::FOLDER_OPEN)).clicked() {
                        if let Some(files) =
                            rfd::FileDialog::new().add_filter("Video", &["mov", "mp4", "m4v", "mkv", "avi"]).pick_files()
                        {
                            self.add_videos(files);
                        }
                    }
                    if ui.button(format!("{}  Add all recordings", ic::VIDEO_CAMERA)).clicked() {
                        let paths: Vec<PathBuf> =
                            self.items.iter().filter(|i| i.cat == Cat::Recordings).map(|i| i.path.clone()).collect();
                        if paths.is_empty() {
                            self.toast("No recordings found yet. Run a scan first", false);
                        }
                        self.add_videos(paths);
                    }
                });
                ui.add_space(6.0);
            });
        });
        if ctx.input(|i| !i.raw.hovered_files.is_empty()) {
            ui.painter().rect_stroke(r.response.rect, 14.0, Stroke::new(2.0_f32, violet));
        }
        ui.add_space(12.0);

        // Options
        let queued = self.jobs.lock().unwrap().iter().filter(|j| matches!(j.status, Status::Queued)).count();
        let mut start = false;
        ui.horizontal(|ui| {
            ui.label(RichText::new("Quality").color(DIM));
            ui.selectable_value(&mut self.opts.quality, Quality::Smallest, "Smallest");
            ui.selectable_value(&mut self.opts.quality, Quality::Balanced, "Balanced");
            ui.selectable_value(&mut self.opts.quality, Quality::Best, "Best");
            ui.add_space(10.0);
            ui.checkbox(&mut self.opts.downscale, "Shrink to 1080p");
            ui.checkbox(&mut self.opts.replace, "Move originals to Trash");
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.add_enabled(!busy, Button::new(format!("{} Clear", ic::X))).clicked() {
                    self.jobs.lock().unwrap().clear();
                }
                start = primary(ui, !busy && queued > 0, format!("{}  Start ({queued})", ic::PLAY), violet).clicked();
            });
        });
        if start {
            *self.encoding.lock().unwrap() = true;
            let (jobs, flag, opts, ctx) = (self.jobs.clone(), self.encoding.clone(), self.opts, ctx.clone());
            std::thread::spawn(move || {
                video::run_queue(jobs, opts, ctx.clone());
                *flag.lock().unwrap() = false;
                ctx.request_repaint();
            });
        }
        ui.add_space(12.0);

        let snapshot = self.jobs.lock().unwrap().clone();
        if snapshot.is_empty() {
            return;
        }
        let mut restore: Option<PathBuf> = None;
        let mut retry: Option<usize> = None;
        card(ui, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
                for (i, j) in snapshot.iter().enumerate() {
                    let name = j.path.file_name().unwrap_or_default().to_string_lossy().to_string();
                    let mode = match (j.mode, j.orig) {
                        (Mode::Restore, Some((w, h, _))) => format!("Restore to {w}×{h}"),
                        _ => "Compress".into(),
                    };
                    ui.allocate_ui_with_layout(vec2(ui.available_width(), 46.0), Layout::right_to_left(Align::Center), |ui| {
                        match &j.status {
                            Status::Queued => {
                                ui.label(RichText::new(format!("{} Waiting", ic::CLOCK)).color(DIM));
                            }
                            Status::Running(p) => {
                                ui.add(ProgressBar::new(*p).desired_width(170.0).show_percentage().fill(violet));
                            }
                            Status::Done(out, size) => {
                                if icon_button(ui, ic::FOLDER_OPEN, DIM, "Show in Finder").clicked() {
                                    system::reveal(out);
                                }
                                if j.mode == Mode::Compress
                                    && ui.small_button(format!("{} Restore size", ic::ARROW_COUNTER_CLOCKWISE)).clicked()
                                {
                                    restore = Some(out.clone());
                                }
                                let pct = 100.0 - *size as f64 / j.size.max(1) as f64 * 100.0;
                                let txt = if j.mode == Mode::Compress {
                                    format!("{} {}  −{pct:.0}%", ic::CHECK_CIRCLE, human(*size))
                                } else {
                                    format!("{} {}", ic::CHECK_CIRCLE, human(*size))
                                };
                                ui.label(RichText::new(txt).color(SUCCESS).strong());
                            }
                            Status::Failed(e) => {
                                if ui.small_button(format!("{} Retry", ic::ARROWS_CLOCKWISE)).clicked() {
                                    retry = Some(i);
                                }
                                ui.add(Label::new(RichText::new(e).color(DANGER).small()).truncate());
                            }
                        }
                        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                            ui.label(RichText::new(ic::FILM_STRIP).size(18.0).color(violet));
                            ui.vertical(|ui| {
                                ui.add(Label::new(RichText::new(&name).color(Color32::WHITE)).truncate());
                                ui.label(RichText::new(format!("{} · {mode}", human(j.size))).small().color(DIM));
                            });
                        });
                    });
                    if i + 1 < snapshot.len() {
                        ui.separator();
                    }
                }
            });
        });
        if let Some(p) = restore {
            self.add_videos(vec![p]);
        }
        if let Some(i) = retry {
            self.jobs.lock().unwrap()[i].status = Status::Queued;
        }
    }

    fn photos_panel(&mut self, ui: &mut Ui, ctx: &egui::Context) {
        let violet = Color32::from_rgb(94, 92, 230);
        let busy = *self.photo_busy.lock().unwrap();
        let r = card(ui, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(6.0);
                ui.label(RichText::new(ic::IMAGES).size(34.0).color(violet));
                ui.label(RichText::new("Drop photos or folders here").size(16.0).strong().color(Color32::WHITE));
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.add_space((ui.available_width() - 330.0).max(0.0) / 2.0);
                    if ui.button(format!("{}  Choose photos", ic::IMAGE)).clicked() {
                        if let Some(files) = rfd::FileDialog::new().add_filter("Photos", &photos::EXTS).pick_files() {
                            self.add_photos(files);
                        }
                    }
                    if ui.button(format!("{}  Choose a folder", ic::FOLDER_OPEN)).on_hover_text("Adds every photo of 300 KB or more inside it").clicked() {
                        if let Some(dir) = rfd::FileDialog::new().set_directory(scan::home().join("Pictures")).pick_folder() {
                            let found = photos::collect(&dir);
                            if found.is_empty() {
                                self.toast("No photos of 300 KB or more in that folder", false);
                            }
                            self.add_photos(found);
                        }
                    }
                });
                ui.add_space(6.0);
            });
        });
        if ctx.input(|i| !i.raw.hovered_files.is_empty()) {
            ui.painter().rect_stroke(r.response.rect, 14.0, Stroke::new(2.0_f32, violet));
        }
        ui.add_space(12.0);

        let queued = self.photo_jobs.lock().unwrap().iter().filter(|j| matches!(j.status, photos::Status::Queued)).count();
        let mut start = false;
        ui.horizontal(|ui| {
            let o = &mut self.photo_opts;
            ui.label(RichText::new("Quality").color(DIM));
            ui.selectable_value(&mut o.quality, Quality::Smallest, "Smallest");
            ui.selectable_value(&mut o.quality, Quality::Balanced, "Balanced");
            ui.selectable_value(&mut o.quality, Quality::Best, "Best");
            ui.add_space(10.0);
            ui.label(RichText::new("Format").color(DIM));
            ui.selectable_value(&mut o.format, photos::Format::Heic, "HEIC").on_hover_text("Smallest. Opens on any Apple device");
            ui.selectable_value(&mut o.format, photos::Format::Jpeg, "JPEG").on_hover_text("Opens everywhere");
            ui.add_space(10.0);
            ui.checkbox(&mut o.replace, "Move originals to Trash");
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.add_enabled(!busy, Button::new(format!("{} Clear", ic::X))).clicked() {
                    self.photo_jobs.lock().unwrap().clear();
                }
                start = primary(ui, !busy && queued > 0, format!("{}  Start ({queued})", ic::PLAY), violet).clicked();
            });
        });
        if start {
            *self.photo_busy.lock().unwrap() = true;
            let (jobs, flag, opts, ctx) = (self.photo_jobs.clone(), self.photo_busy.clone(), self.photo_opts, ctx.clone());
            std::thread::spawn(move || {
                photos::run_queue(jobs, opts, ctx.clone());
                *flag.lock().unwrap() = false;
                ctx.request_repaint();
            });
        }
        ui.add_space(12.0);

        let snapshot = self.photo_jobs.lock().unwrap().clone();
        if snapshot.is_empty() {
            return;
        }
        let (before, after) = snapshot.iter().fold((0u64, 0u64), |(b, a), j| match &j.status {
            photos::Status::Done(_, s) => (b + j.size, a + s),
            _ => (b, a),
        });
        if before > 0 {
            ui.label(RichText::new(format!("{} Saved {} so far ({} → {})", ic::CHECK_CIRCLE, human(before - after), human(before), human(after))).color(SUCCESS));
            ui.add_space(6.0);
        }
        card(ui, |ui| {
            egui::ScrollArea::vertical().id_salt("photo-jobs").auto_shrink([false; 2]).show_rows(ui, 46.0, snapshot.len(), |ui, range| {
                for j in &snapshot[range] {
                    let name = j.path.file_name().unwrap_or_default().to_string_lossy().to_string();
                    ui.allocate_ui_with_layout(vec2(ui.available_width(), 46.0), Layout::right_to_left(Align::Center), |ui| {
                        match &j.status {
                            photos::Status::Queued => {
                                ui.label(RichText::new(format!("{} Waiting", ic::CLOCK)).color(DIM));
                            }
                            photos::Status::Running => {
                                ui.add(egui::Spinner::new().size(14.0).color(violet));
                                ui.label(RichText::new("Compressing…").color(DIM));
                            }
                            photos::Status::Done(out, size) => {
                                if icon_button(ui, ic::FOLDER_OPEN, DIM, "Show in Finder").clicked() {
                                    system::reveal(out);
                                }
                                let pct = 100.0 - *size as f64 / j.size.max(1) as f64 * 100.0;
                                ui.label(RichText::new(format!("{} {}  −{pct:.0}%", ic::CHECK_CIRCLE, human(*size))).color(SUCCESS).strong());
                            }
                            photos::Status::Skipped(why) => {
                                ui.label(RichText::new(why).small().color(DIM));
                            }
                            photos::Status::Failed(e) => {
                                ui.add(Label::new(RichText::new(e).color(DANGER).small()).truncate());
                            }
                        }
                        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                            let mut tui = ui.new_child(egui::UiBuilder::new().max_rect(Rect::from_min_size(ui.cursor().min, vec2(34.0, 34.0))));
                            thumbs::preview(&mut tui, &mut self.thumbs, &j.path, 34.0, ic::IMAGE, violet);
                            ui.add_space(42.0);
                            ui.vertical(|ui| {
                                ui.add(Label::new(RichText::new(&name).color(Color32::WHITE)).truncate());
                                ui.label(RichText::new(format!("{} · {}", human(j.size), short(j.path.parent().unwrap_or(&j.path)))).small().color(DIM));
                            });
                        });
                    });
                }
            });
        });
    }

    fn speed_page(&mut self, ui: &mut Ui) {
        let yellow = Color32::from_rgb(255, 214, 10);
        let live = self.sampler.live.lock().unwrap().clone();
        header(ui, ic::ROCKET_LAUNCH, yellow, "Performance", "Live view of what your Mac is doing, and quick fixes when it feels slow.", |ui| {
            live_dot(ui);
        });

        egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
            // ----- live stats -----
            let disk = self.disk;
            ui.columns(3, |cols| {
                card(&mut cols[0], |ui| {
                    let now = live.cpu.last().copied().unwrap_or(0.0);
                    let c = if now > 0.85 { DANGER } else if now > 0.6 { WARN } else { ACCENT };
                    stat_head(ui, ic::CPU, "CPU", &format!("{:.0}%", now * 100.0), &format!("{} cores", live.cores), c);
                    spark(ui, &live.cpu, c);
                });
                card(&mut cols[1], |ui| {
                    if let Some((used, total)) = live.mem {
                        let f = used as f32 / total.max(1) as f32;
                        let c = if f > 0.9 { DANGER } else if f > 0.75 { WARN } else { SUCCESS };
                        stat_head(ui, ic::MEMORY, "Memory", &human(used), &format!("of {} · {:.0}%", human(total), f * 100.0), c);
                        spark(ui, &live.mem_hist, c);
                    }
                });
                card(&mut cols[2], |ui| {
                    if let Some((free, total)) = disk {
                        let f = 1.0 - free as f32 / total.max(1) as f32;
                        let c = if f > 0.9 { DANGER } else { Color32::from_rgb(100, 210, 255) };
                        stat_head(ui, ic::HARD_DRIVE, "Storage", &human(free), &format!("free of {}", human(total)), c);
                        ui.add_space(14.0);
                        bar(ui, f, c);
                        let note = if free < 20_000_000_000 { "Under 20 GB free slows macOS" } else { "Plenty of room" };
                        ui.label(RichText::new(note).size(11.5).color(if free < 20_000_000_000 { DANGER } else { DIM }));
                    }
                });
            });

            // ----- heaviest apps -----
            ui.add_space(18.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("Heaviest apps right now").size(16.0).strong().color(Color32::WHITE));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if chip(ui, self.by_cpu, &format!("{}  CPU", ic::CPU)) {
                        self.by_cpu = true;
                    }
                    if chip(ui, !self.by_cpu, &format!("{}  Memory", ic::MEMORY)) {
                        self.by_cpu = false;
                    }
                    ui.label(RichText::new("Sort by").small().color(DIM));
                });
            });
            ui.add_space(8.0);
            let mut procs = live.procs.clone();
            if self.by_cpu {
                procs.sort_by(|a, b| b.cpu.total_cmp(&a.cpu));
            } else {
                procs.sort_by(|a, b| b.mem.cmp(&a.mem));
            }
            procs.truncate(8);
            let top_mem = procs.iter().map(|p| p.mem).max().unwrap_or(1).max(1);
            let top_cpu = procs.iter().map(|p| p.cpu).fold(1.0_f32, f32::max);
            let mut kill = None;
            card(ui, |ui| {
                if procs.is_empty() {
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new().size(14.0).color(DIM));
                        ui.label(RichText::new("Measuring…").color(DIM));
                    });
                }
                for (k, p) in procs.iter().enumerate() {
                    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 50.0), Sense::hover());
                    if ui.rect_contains_pointer(rect) {
                        ui.painter().rect_filled(rect.expand2(vec2(6.0, 0.0)), 9.0, Color32::from_white_alpha(7));
                    }
                    let cy = rect.center().y;
                    // icon
                    let ir = Rect::from_min_size(pos2(rect.left(), cy - 17.0), vec2(34.0, 34.0));
                    let mut iui = ui.new_child(egui::UiBuilder::new().max_rect(ir));
                    match &p.app_path {
                        Some(app) => {
                            thumbs::preview(&mut iui, &mut self.thumbs, app, 34.0, ic::APP_WINDOW, ACCENT);
                        }
                        None => {
                            thumbs::preview(&mut iui, &mut self.thumbs, std::path::Path::new(""), 34.0, ic::TERMINAL_WINDOW, DIM);
                        }
                    }
                    // quit button
                    let br = Rect::from_min_max(pos2(rect.right() - 80.0, rect.top()), rect.right_bottom());
                    let mut bui = ui.new_child(egui::UiBuilder::new().max_rect(br).layout(Layout::right_to_left(Align::Center)));
                    if soft(&mut bui, true, format!("{} Quit", ic::X), DANGER).on_hover_text("Quit this app (unsaved work may ask first)").clicked() {
                        kill = Some(k);
                    }
                    // numbers
                    let px = ui.painter();
                    let num_r = rect.right() - 92.0;
                    let cpu_c = if p.cpu > 100.0 { DANGER } else if p.cpu > 40.0 { WARN } else { DIM };
                    px.text(pos2(num_r, cy), Align2::RIGHT_CENTER, format!("{:.0}% CPU", p.cpu), FontId::proportional(12.5), cpu_c);
                    px.text(pos2(num_r - 84.0, cy), Align2::RIGHT_CENTER, human(p.mem), FontId::proportional(14.0), Color32::WHITE);
                    // name + bar
                    let x = rect.left() + 46.0;
                    let w = (num_r - 84.0 - 90.0 - x).max(40.0);
                    let g = truncated(ui, &p.name, 14.0, Color32::WHITE, w);
                    ui.painter().galley(pos2(x, cy - g.size().y - 1.0), g, Color32::WHITE);
                    let track = Rect::from_min_size(pos2(x, cy + 5.0), vec2(w, 5.0));
                    ui.painter().rect_filled(track, 2.5, TRACK);
                    let frac = if self.by_cpu { p.cpu / top_cpu } else { p.mem as f32 / top_mem as f32 };
                    let mut fill = track;
                    fill.set_width((w * frac.clamp(0.0, 1.0)).max(3.0));
                    let bar_c = if self.by_cpu { ACCENT } else { SUCCESS };
                    ui.painter().rect_filled(fill, 2.5, bar_c);
                }
            });
            if let Some(k) = kill {
                system::quit(&procs[k]);
                let name = procs[k].name.clone();
                self.toast(format!("Asked {name} to quit"), true);
            }

            ui.add_space(18.0);
            self.ports_card(ui);

            // ----- quick fixes -----
            ui.add_space(18.0);
            ui.label(RichText::new("Quick fixes").size(16.0).strong().color(Color32::WHITE));
            ui.add_space(8.0);
            let tiles: [(&str, Color32, &str, &str, u8); 4] = [
                (ic::MEMORY, SUCCESS, "Free up RAM", "Clears inactive memory (asks for your password)", 0),
                (ic::GLOBE, ACCENT, "Flush DNS", "Fixes slow or stuck websites", 1),
                (ic::POWER, WARN, "Startup items", "Stop apps launching at login", 2),
                (ic::GAUGE, Color32::from_rgb(191, 90, 242), "Activity Monitor", "See everything that's running", 3),
            ];
            let mut clicked = None;
            for chunk in tiles.chunks(2) {
                ui.columns(2, |cols| {
                    for (col, (icon, color, title, desc, id)) in cols.iter_mut().zip(chunk) {
                        let r = card(col, |ui| {
                            ui.horizontal(|ui| {
                                badge(ui, icon, *color, 38.0);
                                ui.vertical(|ui| {
                                    ui.label(RichText::new(*title).strong().color(Color32::WHITE));
                                    ui.add(Label::new(RichText::new(*desc).small().color(DIM)).truncate().selectable(false));
                                });
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    ui.label(RichText::new(ic::CARET_RIGHT).color(DIM));
                                });
                            });
                        });
                        let resp = col.interact(r.response.rect, Id::new(("fix", *id)), Sense::click());
                        if resp.hovered() {
                            col.painter().rect_stroke(r.response.rect, 14.0, Stroke::new(1.5_f32, *color));
                        }
                        if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                            clicked = Some(*id);
                        }
                    }
                });
                ui.add_space(10.0);
            }
            let admin = |tx: Sender<Msg>, label: &'static str, cmd: &'static str| {
                std::thread::spawn(move || {
                    let msg = match system::admin(cmd) {
                        Ok(()) => format!("{label}: done"),
                        Err(e) => format!("{label}: {e}"),
                    };
                    let _ = tx.send(Msg::Status(msg));
                });
            };
            match clicked {
                Some(0) => admin(self.tx.clone(), "Free up RAM", "/usr/sbin/purge"),
                Some(1) => admin(self.tx.clone(), "Flush DNS", "dscacheutil -flushcache; killall -HUP mDNSResponder"),
                Some(2) => self.page = Page::Startup,
                Some(3) => system::open("/System/Applications/Utilities/Activity Monitor.app"),
                _ => {}
            }

            ui.add_space(8.0);
            card(ui, |ui| {
                ui.horizontal(|ui| {
                    badge(ui, ic::COFFEE, Color32::from_rgb(172, 142, 104), 38.0);
                    ui.vertical(|ui| {
                        ui.label(RichText::new("Keep awake").strong().color(Color32::WHITE));
                        let status = match self.awake {
                            None => "Your Mac sleeps normally".to_string(),
                            Some(0) => "Staying awake until you stop it".to_string(),
                            Some(t) => {
                                let mins = t.saturating_sub(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)) / 60 + 1;
                                format!("Staying awake for {mins} more min")
                            }
                        };
                        ui.label(RichText::new(status).small().color(DIM));
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if self.awake.is_some() {
                            if soft(ui, true, "Stop", DANGER).clicked() {
                                system::stop_awake();
                                self.awake = None;
                            }
                        } else {
                            for (label, secs) in [("Always", None), ("3 h", Some(10_800)), ("1 h", Some(3_600)), ("30 min", Some(1_800))] {
                                if chip(ui, false, label) {
                                    system::keep_awake(secs);
                                    self.awake = system::awake_until();
                                }
                            }
                        }
                    });
                });
            });
            ui.add_space(8.0);
        });
    }
}

impl App {
    /// Apps listening for network connections, refreshed every few seconds in the background.
    fn ports_card(&mut self, ui: &mut Ui) {
        if self.ports_at.map_or(true, |t| t.elapsed() > Duration::from_secs(5)) {
            self.ports_at = Some(Instant::now());
            let (out, ctx) = (self.ports.clone(), ui.ctx().clone());
            std::thread::spawn(move || {
                let v = system::open_ports();
                *out.lock().unwrap() = Some(v);
                ctx.request_repaint();
            });
        }
        let ports = self.ports.lock().unwrap().clone();
        let exposed = ports.as_ref().map_or(0, |v| v.iter().filter(|p| p.exposed()).count());
        ui.horizontal(|ui| {
            ui.label(RichText::new("Open ports").size(16.0).strong().color(Color32::WHITE));
            ui.label(RichText::new("Your apps waiting for network connections").small().color(DIM));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if exposed > 0 {
                    ui.label(RichText::new(format!("{} {exposed} open to your network", ic::WARNING)).small().color(WARN))
                        .on_hover_text("Other devices on the same Wi-Fi can try to connect to these");
                }
            });
        });
        ui.add_space(8.0);
        let mut stop = None;
        card(ui, |ui| {
            let Some(ports) = &ports else {
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new().size(14.0).color(DIM));
                    ui.label(RichText::new("Looking…").color(DIM));
                });
                return;
            };
            if ports.is_empty() {
                ui.label(RichText::new(format!("{}  None of your apps are listening for connections", ic::CHECK_CIRCLE)).color(SUCCESS));
            }
            for (k, p) in ports.iter().enumerate() {
                ui.allocate_ui_with_layout(vec2(ui.available_width(), 34.0), Layout::right_to_left(Align::Center), |ui| {
                    if soft(ui, true, "Stop", DANGER).on_hover_text(format!("Ask {} (PID {}) to quit", p.command, p.pid)).clicked() {
                        stop = Some(p.clone());
                    }
                    if p.proto == "TCP" && icon_btn(ui, ic::GLOBE, &format!("Open http://localhost:{}", p.port), false).clicked() {
                        system::open(&format!("http://localhost:{}", p.port));
                    }
                    let (text, c) = if p.exposed() { ("Open to your network", WARN) } else { ("This Mac only", DIM) };
                    ui.label(RichText::new(text).small().color(c)).on_hover_text(format!("Listening on {}", p.addr));
                    ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                        ui.add_sized(vec2(64.0, 20.0), Label::new(RichText::new(format!(":{}", p.port)).monospace().strong().color(Color32::WHITE)));
                        ui.label(RichText::new(p.proto).small().color(DIM));
                        ui.add_space(6.0);
                        ui.add(Label::new(RichText::new(&p.command).color(Color32::WHITE)).truncate());
                        ui.label(RichText::new(format!("PID {}", p.pid)).small().color(DIM));
                    });
                });
                if k + 1 < ports.len() {
                    ui.separator();
                }
            }
        });
        if let Some(p) = stop {
            system::stop_pid(p.pid);
            self.ports_at = Some(Instant::now() - Duration::from_secs(4));
            self.toast(format!("Asked {} to quit", p.command), true);
        }
    }
}

/// Pill-shaped toggle chip.
fn chip(ui: &mut Ui, on: bool, text: &str) -> bool {
    let fill = if on { ACCENT } else { Color32::from_rgb(44, 44, 49) };
    let fg = if on { Color32::WHITE } else { Color32::from_rgb(210, 210, 216) };
    ui.add(Button::new(RichText::new(text).color(fg)).fill(fill).stroke(Stroke::NONE).rounding(14.0).min_size(vec2(0.0, 28.0)))
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .clicked()
}

/// Pulsing green "Live" marker.
fn live_dot(ui: &mut Ui) {
    let t = ui.input(|i| i.time) as f32;
    let a = 0.55 + 0.45 * (t * 3.0).sin().abs();
    ui.label(RichText::new("Live · updates every second").small().color(DIM));
    let (r, _) = ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
    ui.painter().circle_filled(r.center(), 4.0, SUCCESS.gamma_multiply(a));
    ui.ctx().request_repaint_after(Duration::from_millis(100));
}

/// Card header for a live stat: icon + title, then a big value with a caption.
fn stat_head(ui: &mut Ui, icon: &str, title: &str, value: &str, caption: &str, color: Color32) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(icon).color(color));
        ui.label(RichText::new(title).color(DIM));
    });
    ui.horizontal(|ui| {
        ui.label(RichText::new(value).size(26.0).strong().color(Color32::WHITE));
        ui.label(RichText::new(caption).small().color(DIM));
    });
}

/// Area sparkline of the last minute (values 0..=1).
fn spark(ui: &mut Ui, data: &[f32], color: Color32) {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 46.0), Sense::hover());
    let p = ui.painter();
    p.line_segment([rect.left_bottom(), rect.right_bottom()], Stroke::new(1.0_f32, TRACK));
    if data.len() < 2 {
        p.text(rect.center(), Align2::CENTER_CENTER, "Collecting…", FontId::proportional(11.0), DIM);
        return;
    }
    let n = system::HISTORY.max(data.len()) - 1;
    let pt = |i: usize, v: f32| {
        let x = rect.right() - (data.len() - 1 - i) as f32 / n as f32 * rect.width();
        pos2(x, rect.bottom() - v.clamp(0.0, 1.0) * (rect.height() - 2.0))
    };
    let pts: Vec<Pos2> = data.iter().enumerate().map(|(i, v)| pt(i, *v)).collect();
    let mut mesh = egui::Mesh::default();
    let fill = color.gamma_multiply(0.18);
    for w in pts.windows(2) {
        let base = mesh.vertices.len() as u32;
        mesh.colored_vertex(w[0], fill);
        mesh.colored_vertex(pos2(w[0].x, rect.bottom()), fill);
        mesh.colored_vertex(w[1], fill);
        mesh.colored_vertex(pos2(w[1].x, rect.bottom()), fill);
        mesh.add_triangle(base, base + 1, base + 2);
        mesh.add_triangle(base + 1, base + 3, base + 2);
    }
    p.add(Shape::mesh(mesh));
    p.add(Shape::line(pts.clone(), Stroke::new(2.0_f32, color)));
    p.circle_filled(*pts.last().unwrap(), 3.0, color);
    if let Some(pos) = resp.hover_pos() {
        let i = pts.iter().enumerate().min_by(|a, b| (a.1.x - pos.x).abs().total_cmp(&(b.1.x - pos.x).abs())).map(|(i, _)| i).unwrap_or(0);
        p.line_segment([pos2(pts[i].x, rect.top()), pos2(pts[i].x, rect.bottom())], Stroke::new(1.0_f32, Color32::from_white_alpha(40)));
        p.circle_filled(pts[i], 3.5, Color32::WHITE);
        let secs = data.len() - 1 - i;
        resp.on_hover_text(format!("{:.0}% · {}", data[i] * 100.0, if secs == 0 { "now".to_string() } else { format!("{secs}s ago") }));
    }
}

fn setup_style(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
    ctx.set_fonts(fonts);

    ctx.set_theme(egui::ThemePreference::Dark);
    let mut v = egui::Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = CARD;
    v.extreme_bg_color = Color32::from_rgb(26, 26, 29);
    v.selection.bg_fill = ACCENT;
    v.hyperlink_color = ACCENT;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, BORDER);
    v.widgets.noninteractive.fg_stroke.color = Color32::from_rgb(210, 210, 215);
    for (w, fill) in [
        (&mut v.widgets.inactive, Color32::from_rgb(44, 44, 49)),
        (&mut v.widgets.hovered, Color32::from_rgb(56, 56, 62)),
        (&mut v.widgets.active, Color32::from_rgb(66, 66, 72)),
    ] {
        w.weak_bg_fill = fill;
        w.bg_fill = fill;
        w.rounding = 8.0.into();
    }
    ctx.set_visuals_of(egui::Theme::Dark, v);
    ctx.style_mut_of(egui::Theme::Dark, |s| {
        s.spacing.item_spacing = vec2(8.0, 6.0);
        s.spacing.button_padding = vec2(12.0, 6.0);
        s.spacing.interact_size.y = 28.0;
    });
}

fn main() -> eframe::Result {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(|s| s.as_str()) {
        Some("--charge-daemon") => battery::run_daemon(),
        Some("--charge-reset") => {
            battery::reset_charging();
            return Ok(());
        }
        Some("--agent") => agent::run_agent(),
        Some("--plug-in") => return agent::run_plug_in(args.iter().any(|a| a == "--preview")),
        _ => {}
    }
    if args.get(1).map(|s| s.as_str()) == Some("--smc-read") {
        let smc = smc::Smc::open().expect("open SMC");
        for k in &args[2..] {
            println!("{k}: {:?}", smc.read(k));
        }
        println!("charge key: {:?}, charging allowed: {:?}", smc::charge_key(&smc), smc::charging_allowed(&smc));
        return Ok(());
    }
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Clean You")
            .with_icon(eframe::icon_data::from_png_bytes(include_bytes!("../assets/app-icon.png")).expect("valid application icon"))
            .with_inner_size([1180.0, 760.0])
            .with_min_inner_size([900.0, 560.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native(
        "Clean You",
        options,
        Box::new(|cc| {
            setup_style(&cc.egui_ctx);
            Ok(Box::new(App::new(&cc.egui_ctx)))
        }),
    )
}

impl App {
    /// The whole drive on one bar: what could be cleared (in color, per category) next to
    /// what is kept (gray) and what is free.
    fn overview_card(&self, ui: &mut Ui) {
        let Some((free, total)) = self.disk else { return };
        let used = total.saturating_sub(free);
        let segs: Vec<(&str, u64, Color32)> = Cat::ALL
            .iter()
            .filter(|c| **c != Cat::Recordings)
            .map(|c| (c.title(), self.total(*c).1, cat_color(*c)))
            .filter(|s| s.1 > 0)
            .collect();
        let clear: u64 = segs.iter().map(|s| s.1).sum::<u64>().min(used);
        let safe: u64 = self.items.iter().filter(|i| i.safe).map(|i| i.size).sum::<u64>().min(clear);
        let kept = used - clear;
        let kept_c = Color32::from_rgb(92, 92, 100);
        card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("{}  Macintosh HD", ic::HARD_DRIVE)).strong().color(Color32::WHITE));
                ui.label(RichText::new(format!("{} free of {}", human(free), human(total))).color(DIM));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(RichText::new(format!("{} you could clear", human(clear))).strong().color(SUCCESS));
                });
            });
            ui.add_space(8.0);
            let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 22.0), Sense::hover());
            let p = ui.painter();
            p.rect_filled(rect, 6.0, TRACK);
            let mut x = rect.left();
            let mut tip = None;
            let parts = segs.iter().map(|s| (s.0, s.1, s.2)).chain([("Kept (apps, photos, documents, system)", kept, kept_c)]);
            for (name, size, color) in parts {
                let w = rect.width() * size as f32 / total.max(1) as f32;
                if w < 0.5 {
                    continue;
                }
                let r = Rect::from_min_max(pos2(x, rect.top()), pos2((x + w).min(rect.right()), rect.bottom()));
                p.rect_filled(r, 2.0, color);
                if resp.hover_pos().is_some_and(|h| r.contains(h)) {
                    tip = Some(format!("{name} · {}", human(size)));
                }
                x += w;
            }
            if let Some(t) = tip {
                resp.on_hover_text(t);
            }
            ui.add_space(8.0);
            ui.horizontal_wrapped(|ui| {
                let key = |ui: &mut Ui, c: Color32, text: String| {
                    let (r, _) = ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
                    ui.painter().rect_filled(r, 3.0, c);
                    ui.label(RichText::new(text).small().color(DIM));
                    ui.add_space(8.0);
                };
                key(ui, SUCCESS, format!("Safe to clear {}", human(safe)));
                key(ui, WARN, format!("Worth a look {}", human(clear - safe)));
                key(ui, kept_c, format!("Kept {}", human(kept)));
                key(ui, TRACK, format!("Free {}", human(free)));
            });
        });
    }

    /// "Freed this week" card with an 8-week bar chart from the cleanup history.
    fn activity_card(&mut self, ui: &mut Ui) {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let hist = scan::history();
        let week = 7 * 86_400;
        let weeks: Vec<u64> = (0..8u64)
            .rev()
            .map(|w| {
                let end = now - w * week;
                hist.iter().filter(|(t, _)| *t > end.saturating_sub(week) && *t <= end).map(|(_, b)| b).sum()
            })
            .collect();
        let total: u64 = hist.iter().map(|(_, b)| b).sum();
        card(ui, |ui| {
            ui.horizontal(|ui| {
                badge(ui, ic::CHART_BAR, SUCCESS, 38.0);
                ui.vertical(|ui| {
                    ui.label(RichText::new("Activity").color(DIM));
                    ui.label(RichText::new(format!("{} freed this week", human(*weeks.last().unwrap_or(&0)))).size(18.0).strong().color(Color32::WHITE));
                    ui.label(RichText::new(format!("{} since you started using Clean You", human(total))).small().color(DIM));
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let (rect, _) = ui.allocate_exact_size(vec2(8.0 * 18.0, 54.0), Sense::hover());
                    let max = weeks.iter().copied().max().unwrap_or(0).max(1);
                    for (i, b) in weeks.iter().enumerate() {
                        let h = (*b as f32 / max as f32 * 46.0).max(3.0);
                        let x = rect.left() + i as f32 * 18.0;
                        let r = Rect::from_min_max(pos2(x + 3.0, rect.bottom() - h), pos2(x + 15.0, rect.bottom()));
                        let c = if i == 7 { SUCCESS } else { SUCCESS.gamma_multiply(0.4) };
                        ui.painter().rect_filled(r, 3.0, c);
                    }
                });
            });
        });
    }
}
