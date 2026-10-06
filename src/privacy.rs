//! Privacy cleanup: browser history, recent items, download log.

use super::*;
use std::collections::HashMap;
use std::path::Path;
use std::process::{Command, Stdio};

const FDA_SETTINGS: &str = "x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles";
const QUARANTINE: &str = "Library/Preferences/com.apple.LaunchServices.QuarantineEventsV2";
const SHARED_FILE_LIST: &str = "Library/Application Support/com.apple.sharedfilelist";
const RECENT_PATTERNS: [&str; 4] = ["RecentDocuments", "RecentApplications", "RecentServers", "RecentHosts"];
const HISTORY_FILES: [&str; 5] = ["History", "History-journal", "Visited Links", "Top Sites", "Shortcuts"];
const COOKIE_FILES: [&str; 3] = ["Cookies", "Cookies-journal", "Network/Cookies"];

/// (display name, process name for pgrep, data folder under ~/Library/Application Support)
const CHROMIUM: [(&str, &str, &str); 5] = [
    ("Chrome", "Google Chrome", "Google/Chrome"),
    ("Brave", "Brave Browser", "BraveSoftware/Brave-Browser"),
    ("Edge", "Microsoft Edge", "Microsoft Edge"),
    ("Arc", "Arc", "Arc/User Data"),
    ("Vivaldi", "Vivaldi", "Vivaldi"),
];

#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    Quarantine,
    Recent,
    Files,
    Clipboard,
    /// Shown but can't be cleaned (Safari).
    Locked,
}

#[derive(Clone)]
struct Trace {
    key: String,
    icon: &'static str,
    color: Color32,
    title: String,
    desc: String,
    kind: Kind,
    paths: Vec<PathBuf>,
    size: u64,
    /// What's there, e.g. "3,327 downloads" or "4 files · 2.1 MB".
    detail: String,
    /// Nothing to clean.
    empty: bool,
    /// Why it can't be cleaned right now ("Quit Chrome first").
    blocked: Option<String>,
    default_on: bool,
    warn: Option<&'static str>,
}

#[derive(Default)]
pub struct State {
    last_frame: Option<u64>,
    loading: bool,
    cleaning: bool,
    incoming: Arc<Mutex<Option<Vec<Trace>>>>,
    traces: Vec<Trace>,
    checked: HashMap<String, bool>,
}

// ---------- pure helpers ----------

pub fn is_recent_list(name: &str) -> bool {
    RECENT_PATTERNS.iter().any(|p| name.contains(p))
}

fn is_sfl(p: &Path) -> bool {
    p.extension().is_some_and(|e| e == "sfl2" || e == "sfl3")
}

/// Recent-items lists in the sharedfilelist folder (and its per-app subfolders).
pub fn recent_files(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut out = vec![];
    for e in std::fs::read_dir(dir)?.flatten() {
        let p = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        if p.is_dir() && !p.is_symlink() {
            let parent_match = is_recent_list(&name);
            if let Ok(rd) = std::fs::read_dir(&p) {
                for c in rd.flatten() {
                    let cp = c.path();
                    if is_sfl(&cp) && (parent_match || is_recent_list(&c.file_name().to_string_lossy())) {
                        out.push(cp);
                    }
                }
            }
        } else if is_sfl(&p) && is_recent_list(&name) {
            out.push(p);
        }
    }
    out.sort();
    Ok(out)
}

/// "Default", "Profile 1", ... inside a Chromium user-data folder.
pub fn chromium_profiles(root: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(root) else { return vec![] };
    let mut v: Vec<PathBuf> = rd
        .flatten()
        .filter(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            e.path().is_dir() && (n == "Default" || n.strip_prefix("Profile ").is_some_and(|r| !r.is_empty() && r.chars().all(|c| c.is_ascii_digit())))
        })
        .map(|e| e.path())
        .collect();
    v.sort();
    v
}

/// The named files that exist in any of the profiles.
pub fn existing_in(profiles: &[PathBuf], names: &[&str]) -> Vec<PathBuf> {
    profiles.iter().flat_map(|p| names.iter().map(move |n| p.join(n))).filter(|p| p.is_file()).collect()
}

/// sqlite3 prints a bare number.
pub fn parse_count(out: &str) -> Option<u64> {
    out.trim().parse().ok()
}

/// 3327 -> "3,327"
pub fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn plural(n: u64, one: &str, many: &str) -> String {
    format!("{} {}", thousands(n), if n == 1 { one } else { many })
}

fn files_detail(paths: &[PathBuf], size: u64) -> String {
    if paths.is_empty() {
        "Nothing to clean".into()
    } else {
        format!("{} · {}", plural(paths.len() as u64, "file", "files"), human(size))
    }
}

fn sizes(paths: &[PathBuf]) -> u64 {
    paths.iter().map(|p| scan::dir_size(p)).sum()
}

fn running(process: &str) -> bool {
    Command::new("/usr/bin/pgrep").args(["-x", process]).stdout(Stdio::null()).status().is_ok_and(|s| s.success())
}

fn sqlite(db: &Path, sql: &str) -> Result<String, String> {
    let o = Command::new("/usr/bin/sqlite3").arg(db).arg(sql).output().map_err(|e| e.to_string())?;
    if o.status.success() {
        Ok(String::from_utf8_lossy(&o.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&o.stderr).trim().to_string())
    }
}

// ---------- gathering (background thread) ----------

fn base(key: &str, icon: &'static str, color: Color32, title: &str, desc: &str, kind: Kind) -> Trace {
    Trace {
        key: key.into(),
        icon,
        color,
        title: title.into(),
        desc: desc.into(),
        kind,
        paths: vec![],
        size: 0,
        detail: String::new(),
        empty: false,
        blocked: None,
        default_on: false,
        warn: None,
    }
}

fn files_trace(mut t: Trace, paths: Vec<PathBuf>) -> Trace {
    t.size = sizes(&paths);
    t.detail = files_detail(&paths, t.size);
    t.empty = paths.is_empty();
    t.paths = paths;
    t
}

fn gather() -> Vec<Trace> {
    let home = scan::home();
    let mut v = vec![];

    // Download log
    let db = home.join(QUARANTINE);
    let mut t = base("downloads", ic::DOWNLOAD_SIMPLE, ACCENT, "Download history log",
        "macOS quietly keeps a record of every file you've ever downloaded, with the web address it came from.", Kind::Quarantine);
    t.default_on = true;
    if db.exists() {
        match sqlite(&db, "select count(*) from LSQuarantineEvent").ok().and_then(|o| parse_count(&o)) {
            Some(n) => {
                t.detail = plural(n, "download recorded", "downloads recorded");
                t.empty = n == 0;
            }
            None => {
                t.detail = "Couldn't read the log".into();
                t.blocked = Some("Not readable".into());
            }
        }
        t.paths = vec![db];
    } else {
        t.detail = "Nothing to clean".into();
        t.empty = true;
    }
    v.push(t);

    // Recent items
    let mut t = base("recent", ic::CLOCK_COUNTER_CLOCKWISE, Color32::from_rgb(100, 210, 255), "Recent items lists",
        "Recent documents, apps and servers shown in the Apple menu, Finder and each app's Open Recent menu.", Kind::Recent);
    t.default_on = true;
    match recent_files(&home.join(SHARED_FILE_LIST)) {
        Ok(paths) => t = files_trace(t, paths),
        Err(_) => {
            t.detail = "macOS blocks access to this folder".into();
            t.blocked = Some("Needs Full Disk Access".into());
        }
    }
    v.push(t);

    // Chromium browsers
    let support = home.join("Library/Application Support");
    for (name, process, rel) in CHROMIUM {
        let profiles = chromium_profiles(&support.join(rel));
        if profiles.is_empty() {
            continue;
        }
        let blocked = running(process).then(|| format!("Quit {name} first"));
        let key = name.to_lowercase();
        let mut h = files_trace(
            base(&format!("{key}-history"), ic::GLOBE, Color32::from_rgb(255, 159, 10), &format!("{name} browsing history"),
                "Sites you visited, typed-address suggestions and the most-visited tiles. Bookmarks and passwords stay.", Kind::Files),
            existing_in(&profiles, &HISTORY_FILES),
        );
        h.blocked = blocked.clone();
        v.push(h);
        let mut c = files_trace(
            base(&format!("{key}-cookies"), ic::COOKIE, Color32::from_rgb(255, 214, 10), &format!("{name} cookies (signs you out of sites)"),
                "Removes trackers and saved logins on websites. You'll need to sign in again everywhere.", Kind::Files),
            existing_in(&profiles, &COOKIE_FILES),
        );
        c.blocked = blocked;
        v.push(c);
    }

    // Firefox caches only (never places.sqlite)
    let ff = home.join("Library/Caches/Firefox");
    if ff.is_dir() {
        let paths: Vec<PathBuf> = std::fs::read_dir(&ff).map(|rd| rd.flatten().map(|e| e.path()).collect()).unwrap_or_default();
        let mut t = files_trace(
            base("firefox-cache", ic::FIRE, Color32::from_rgb(255, 120, 50), "Firefox cache",
                "Copies of pages and images Firefox saved while browsing. History and bookmarks stay.", Kind::Files),
            paths,
        );
        t.blocked = running("firefox").then(|| "Quit Firefox first".into());
        v.push(t);
    }

    // Safari (protected by macOS)
    let mut t = base("safari", ic::COMPASS, ACCENT, "Safari history",
        "macOS protects Safari's data. Clear it in Safari › History › Clear History.", Kind::Locked);
    t.detail = "Protected by macOS".into();
    t.blocked = Some("Needs Full Disk Access".into());
    v.push(t);

    // Shell history
    let shells: Vec<PathBuf> = [".zsh_history", ".bash_history"].iter().map(|f| home.join(f)).filter(|p| p.is_file()).collect();
    let mut t = files_trace(
        base("shell", ic::TERMINAL_WINDOW, DIM, "Terminal command history",
            "Commands you've typed in Terminal (zsh and bash).", Kind::Files),
        shells,
    );
    t.warn = Some("You'll lose the ↑ arrow recall of past commands. This can't be undone.");
    v.push(t);

    // Clipboard
    let mut t = base("clipboard", ic::CLIPBOARD_TEXT, Color32::from_rgb(191, 90, 242), "Clipboard",
        "Whatever you last copied, like a password or address.", Kind::Clipboard);
    let n = Command::new("/usr/bin/pbpaste").output().map(|o| String::from_utf8_lossy(&o.stdout).chars().count() as u64).unwrap_or(0);
    t.detail = if n > 0 { plural(n, "character copied", "characters copied") } else { "No text copied".into() };
    v.push(t);

    v
}

/// Cleans one trace. Returns bytes freed.
fn clean(t: &Trace) -> Result<u64, String> {
    match t.kind {
        Kind::Quarantine => {
            let db = t.paths.first().ok_or("no log")?;
            let before = scan::dir_size(db);
            sqlite(db, "delete from LSQuarantineEvent; vacuum;")?;
            Ok(before.saturating_sub(scan::dir_size(db)))
        }
        Kind::Recent | Kind::Files => {
            let mut freed = 0;
            let mut err = None;
            for p in &t.paths {
                let s = scan::dir_size(p);
                match scan::remove(p, true) {
                    Ok(()) => freed += s,
                    Err(e) => err = Some(e),
                }
            }
            if t.kind == Kind::Recent {
                let _ = Command::new("/usr/bin/killall").arg("sharedfilelistd").stdout(Stdio::null()).stderr(Stdio::null()).status();
            }
            match err {
                Some(e) if freed == 0 => Err(e),
                _ => Ok(freed),
            }
        }
        Kind::Clipboard => {
            let ok = Command::new("/usr/bin/pbcopy").stdin(Stdio::null()).status().is_ok_and(|s| s.success());
            if ok {
                Ok(0)
            } else {
                Err("couldn't clear".into())
            }
        }
        Kind::Locked => Err("protected by macOS".into()),
    }
}

fn can_clean(t: &Trace) -> bool {
    t.kind != Kind::Locked && t.blocked.is_none() && (!t.empty || t.kind == Kind::Clipboard)
}

// ---------- UI ----------

const GREEN: Color32 = Color32::from_rgb(48, 209, 88);

fn checkbox(ui: &mut Ui, on: bool, enabled: bool) -> bool {
    let (rect, resp) = ui.allocate_exact_size(vec2(22.0, 22.0), if enabled { Sense::click() } else { Sense::hover() });
    let p = ui.painter();
    if on && enabled {
        p.rect_filled(rect, 6.0, GREEN);
        p.text(rect.center(), Align2::CENTER_CENTER, ic::CHECK, FontId::proportional(14.0), Color32::WHITE);
    } else {
        p.rect_stroke(rect.shrink(1.0), 6.0, Stroke::new(1.5_f32, if enabled { DIM } else { TRACK }));
    }
    enabled && resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
}

impl App {
    fn privacy_refresh(&mut self, ctx: &egui::Context) {
        if self.privacy.loading {
            return;
        }
        self.privacy.loading = true;
        let (slot, ctx) = (self.privacy.incoming.clone(), ctx.clone());
        std::thread::spawn(move || {
            *slot.lock().unwrap() = Some(gather());
            ctx.request_repaint();
        });
    }

    fn privacy_clean(&mut self, ctx: &egui::Context) {
        let todo: Vec<Trace> = self.privacy.traces.iter().filter(|t| self.privacy_checked(t) && can_clean(t)).cloned().collect();
        if todo.is_empty() || self.privacy.cleaning {
            return;
        }
        self.privacy.cleaning = true;
        self.privacy.loading = true;
        let (tx, slot, ctx) = (self.tx.clone(), self.privacy.incoming.clone(), ctx.clone());
        std::thread::spawn(move || {
            let (mut done, mut freed, mut errs) = (0, 0u64, vec![]);
            for t in &todo {
                match clean(t) {
                    Ok(f) => {
                        done += 1;
                        freed += f;
                    }
                    Err(e) => errs.push(format!("{}: {e}", t.title)),
                }
            }
            let mut msg = format!("Cleaned {}", plural(done, "item", "items"));
            if freed > 0 {
                msg += &format!(" · freed {}", human(freed));
            }
            if let Some(e) = errs.first() {
                msg += &format!(" · {e}");
                if errs.len() > 1 {
                    msg += &format!(" (+{} more)", errs.len() - 1);
                }
            }
            let _ = tx.send(Msg::Status(msg));
            *slot.lock().unwrap() = Some(gather());
            ctx.request_repaint();
        });
    }

    fn privacy_checked(&self, t: &Trace) -> bool {
        self.privacy.checked.get(&t.key).copied().unwrap_or(t.default_on)
    }

    pub(crate) fn privacy_page(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        let f = ctx.cumulative_pass_nr();
        if self.privacy.last_frame.map_or(true, |l| f > l + 1) {
            self.privacy_refresh(&ctx);
        }
        self.privacy.last_frame = Some(f);
        if let Some(v) = self.privacy.incoming.lock().unwrap().take() {
            self.privacy.traces = v;
            self.privacy.loading = false;
            self.privacy.cleaning = false;
        }

        let selected: Vec<&Trace> = self.privacy.traces.iter().filter(|t| self.privacy_checked(t) && can_clean(t)).collect();
        let n_sel = selected.len();
        let sel_size: u64 = selected.iter().map(|t| t.size).sum();
        let (loading, cleaning) = (self.privacy.loading, self.privacy.cleaning);
        let mut go = false;
        let mut refresh = false;
        header(ui, ic::EYE_SLASH, ACCENT, "Privacy", "Erase the traces your Mac and browsers keep about what you do.", |ui| {
            let label = if cleaning { "Cleaning…".to_string() } else { format!("{}  Clean selected", ic::BROOM) };
            if primary(ui, n_sel > 0 && !cleaning, label, ACCENT).clicked() {
                go = true;
            }
            ui.add_space(6.0);
            if loading {
                ui.add(egui::Spinner::new().size(16.0).color(DIM));
            } else if icon_button(ui, ic::ARROWS_CLOCKWISE, DIM, "Refresh").clicked() {
                refresh = true;
            }
        });
        if go {
            self.privacy_clean(&ctx);
        }
        if refresh {
            self.privacy_refresh(&ctx);
        }

        let mut toggled: Option<(String, bool)> = None;
        let mut open_fda = false;
        egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
            card(ui, |ui| {
                ui.horizontal(|ui| {
                    badge(ui, ic::SHIELD_CHECK, GREEN, 40.0);
                    ui.vertical(|ui| {
                        let t = if self.privacy.traces.is_empty() {
                            "Looking for privacy traces…".to_string()
                        } else if n_sel == 0 {
                            "Nothing selected".to_string()
                        } else {
                            format!("{} selected", plural(n_sel as u64, "item", "items"))
                        };
                        ui.label(RichText::new(t).size(18.0).strong().color(Color32::WHITE));
                        let sub = if sel_size > 0 { format!("{} on disk. Cleaning is permanent.", human(sel_size)) } else { "Cleaning is permanent.".into() };
                        ui.label(RichText::new(sub).color(DIM));
                    });
                });
            });
            if cleaning {
                ui.add_space(8.0);
                indeterminate(ui, ACCENT);
            }
            ui.add_space(12.0);

            card(ui, |ui| {
                for (i, t) in self.privacy.traces.iter().enumerate() {
                    if i > 0 {
                        ui.separator();
                    }
                    let enabled = can_clean(t) && !cleaning;
                    let on = self.privacy.checked.get(&t.key).copied().unwrap_or(t.default_on);
                    ui.horizontal(|ui| {
                        ui.add_space(2.0);
                        if checkbox(ui, on && can_clean(t), enabled) {
                            toggled = Some((t.key.clone(), !on));
                        }
                        ui.add_space(6.0);
                        badge(ui, t.icon, if t.kind == Kind::Locked { DIM } else { t.color }, 34.0);
                        ui.vertical(|ui| {
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(&t.title).strong().color(if t.kind == Kind::Locked { DIM } else { Color32::WHITE }));
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    if t.kind == Kind::Locked {
                                        if ui.small_button(format!("{} Full Disk Access", ic::LOCK)).clicked() {
                                            open_fda = true;
                                        }
                                    } else if let Some(b) = &t.blocked {
                                        ui.label(RichText::new(format!("{} {b}", ic::WARNING)).small().color(WARN));
                                    } else {
                                        let c = if t.empty && t.kind != Kind::Clipboard { DIM } else { Color32::from_gray(210) };
                                        ui.label(RichText::new(&t.detail).small().color(c));
                                    }
                                });
                            });
                            ui.add(Label::new(RichText::new(&t.desc).small().color(DIM)).wrap());
                            if let Some(w) = t.warn {
                                ui.label(RichText::new(format!("{} {w}", ic::WARNING)).small().color(WARN));
                            }
                            if t.blocked.as_deref() == Some("Needs Full Disk Access") && t.kind != Kind::Locked && ui.link(RichText::new("Open Full Disk Access settings").small()).clicked() {
                                open_fda = true;
                            }
                        });
                    });
                    ui.add_space(2.0);
                }
                if self.privacy.traces.is_empty() {
                    ui.label(RichText::new("Loading…").color(DIM));
                }
            });
        });

        if let Some((k, v)) = toggled {
            self.privacy.checked.insert(k, v);
        }
        if open_fda {
            system::open(FDA_SETTINGS);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cleanyou-privacy-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn matches_recent_lists() {
        assert!(is_recent_list("com.apple.LSSharedFileList.RecentDocuments.sfl3"));
        assert!(is_recent_list("com.apple.LSSharedFileList.RecentServers.sfl2"));
        assert!(!is_recent_list("com.apple.LSSharedFileList.FavoriteItems.sfl3"));

        let d = tmp("sfl");
        for f in [
            "com.apple.LSSharedFileList.RecentDocuments.sfl3",
            "com.apple.LSSharedFileList.RecentHosts.sfl2",
            "com.apple.LSSharedFileList.FavoriteItems.sfl3",
            "com.apple.LSSharedFileList.RecentApplications.txt",
        ] {
            std::fs::write(d.join(f), "x").unwrap();
        }
        let sub = d.join("com.apple.LSSharedFileList.ApplicationRecentDocuments");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("com.apple.textedit.sfl3"), "x").unwrap();
        let other = d.join("com.apple.LSSharedFileList.Other");
        std::fs::create_dir(&other).unwrap();
        std::fs::write(other.join("x.sfl3"), "x").unwrap();

        let names: Vec<String> = recent_files(&d).unwrap().iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
        std::fs::remove_dir_all(&d).unwrap();
        assert_eq!(names.len(), 3, "{names:?}");
        assert!(names.contains(&"com.apple.textedit.sfl3".to_string()));
        assert!(!names.iter().any(|n| n.contains("Favorite") || n == "x.sfl3"));
        assert!(recent_files(Path::new("/nonexistent/clean-you")).is_err());
    }

    #[test]
    fn finds_chromium_profiles_and_files() {
        let d = tmp("chrome");
        for p in ["Default", "Profile 1", "Profile 12", "Profile x", "Guest Profile", "System Profile"] {
            std::fs::create_dir_all(d.join(p)).unwrap();
        }
        std::fs::write(d.join("Local State"), "{}").unwrap();
        std::fs::write(d.join("Default/History"), "h").unwrap();
        std::fs::write(d.join("Default/Bookmarks"), "b").unwrap();
        std::fs::create_dir_all(d.join("Profile 1/Network")).unwrap();
        std::fs::write(d.join("Profile 1/Network/Cookies"), "c").unwrap();
        std::fs::write(d.join("Profile 1/Top Sites"), "t").unwrap();

        let profiles = chromium_profiles(&d);
        let names: Vec<String> = profiles.iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(names, vec!["Default", "Profile 1", "Profile 12"]);
        let hist = existing_in(&profiles, &HISTORY_FILES);
        let cookies = existing_in(&profiles, &COOKIE_FILES);
        std::fs::remove_dir_all(&d).unwrap();
        assert_eq!(hist.len(), 2);
        assert!(hist.iter().all(|p| !p.ends_with("Bookmarks")));
        assert_eq!(cookies.len(), 1);
        assert!(cookies[0].ends_with("Network/Cookies"));
        assert!(chromium_profiles(Path::new("/nonexistent/clean-you")).is_empty());
    }

    #[test]
    fn formats_numbers() {
        assert_eq!(parse_count("3327\n"), Some(3327));
        assert_eq!(parse_count("Error: no such table"), None);
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(3327), "3,327");
        assert_eq!(thousands(1234567), "1,234,567");
        assert_eq!(plural(1, "file", "files"), "1 file");
        assert_eq!(files_detail(&[], 0), "Nothing to clean");
    }

    #[test]
    fn cleans_files_in_temp_dir() {
        let d = tmp("clean");
        let a = d.join("History");
        std::fs::write(&a, vec![0u8; 5000]).unwrap();
        let mut t = files_trace(base("x", ic::GLOBE, ACCENT, "x", "x", Kind::Files), vec![a.clone()]);
        assert!(!t.empty && can_clean(&t));
        assert!(clean(&t).unwrap() > 0);
        assert!(!a.exists());
        t.blocked = Some("Quit X first".into());
        assert!(!can_clean(&t));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn cleans_quarantine_db_copy() {
        let d = tmp("q");
        let db = d.join("q.db");
        sqlite(&db, "create table LSQuarantineEvent (x text); insert into LSQuarantineEvent values ('a'),('b');").unwrap();
        assert_eq!(sqlite(&db, "select count(*) from LSQuarantineEvent").ok().and_then(|o| parse_count(&o)), Some(2));
        let mut t = base("downloads", ic::GLOBE, ACCENT, "x", "x", Kind::Quarantine);
        t.paths = vec![db.clone()];
        clean(&t).unwrap();
        assert_eq!(sqlite(&db, "select count(*) from LSQuarantineEvent").ok().and_then(|o| parse_count(&o)), Some(0));
        std::fs::remove_dir_all(&d).unwrap();
    }

    /// Read-only look at this Mac: `cargo test -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn smoke_gather() {
        for t in gather() {
            println!("{:<40} {:<32} blocked={:?} files={}", t.title, t.detail, t.blocked, t.paths.len());
        }
    }
}
