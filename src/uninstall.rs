//! App Uninstaller: removes apps together with their leftover files.

use super::*;
use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

const DAY: i64 = 86_400;
const LARGEST: usize = 15;

/// ~/Library folders that can hold an app's files.
const LIB_DIRS: [&str; 12] = [
    "Application Support",
    "Caches",
    "Preferences",
    "Containers",
    "Group Containers",
    "Saved Application State",
    "Logs",
    "HTTPStorages",
    "WebKit",
    "Application Scripts",
    "LaunchAgents",
    "Cookies",
];

/// Folders searched for leftovers of apps that are no longer installed.
const ORPHAN_DIRS: [&str; 6] =
    ["Containers", "Saved Application State", "Caches", "HTTPStorages", "WebKit", "Application Support"];

/// Bundle-id prefixes that are never reported as orphans (system, or shared frameworks used by many apps).
const NEVER_ORPHAN: [&str; 12] = [
    "com.apple.",
    "group.com.apple.",
    "systemgroup.",
    "org.sparkle-project.",
    "com.plausiblelabs.",
    "io.sentry",
    "com.crashlytics",
    "org.swift.",
    "com.google.keystone",
    "com.github.electron",
    "org.webkit.playwright",
    "org.chromium.",
];

#[derive(Clone, Copy, PartialEq, Default)]
enum Filter {
    #[default]
    All,
    Unused,
    Largest,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Used {
    Loading,
    /// Spotlight has no data at all (indexing off), so we can't tell.
    Unknown,
    Never,
    At(i64),
}

struct AppInfo {
    path: PathBuf,
    name: String,
    id: String,
    version: String,
    size: u64,
    leftovers: Vec<(PathBuf, u64)>,
    /// Owned by root / another user: moving it needs the admin password.
    locked: bool,
    used: Used,
}

impl AppInfo {
    fn total(&self) -> u64 {
        self.size + self.leftovers.iter().map(|l| l.1).sum::<u64>()
    }
}

struct Orphan {
    path: PathBuf,
    id: String,
    size: u64,
    selected: bool,
}

/// Written by the scan thread, taken by the UI.
#[derive(Default)]
struct Shared {
    apps: Option<Vec<AppInfo>>,
    orphans: Option<Vec<Orphan>>,
    used: Option<Vec<(PathBuf, Used)>>,
    done: bool,
}

#[derive(Default)]
pub struct State {
    shared: Arc<Mutex<Shared>>,
    /// Finished admin uninstalls: (app, paths that were moved away).
    admin_done: Arc<Mutex<Vec<(PathBuf, Vec<PathBuf>)>>>,
    admin_busy: HashSet<PathBuf>,
    admin_ask: Option<PathBuf>,
    started: bool,
    scanning: bool,
    apps: Vec<AppInfo>,
    orphans: Vec<Orphan>,
    query: String,
    filter: Filter,
    open: HashSet<PathBuf>,
    seen_removed: usize,
}

impl State {
    fn rescan(&mut self) {
        self.started = true;
        self.scanning = true;
        self.shared = Arc::new(Mutex::new(Shared::default()));
        let shared = self.shared.clone();
        std::thread::spawn(move || scan_apps(shared));
    }

    /// Picks up background results; returns paths moved away by admin uninstalls.
    fn poll(&mut self) -> Vec<PathBuf> {
        if let Ok(mut s) = self.shared.try_lock() {
            if let Some(a) = s.apps.take() {
                self.apps = a;
            }
            if let Some(o) = s.orphans.take() {
                self.orphans = o;
            }
            if let Some(u) = s.used.take() {
                let m: HashMap<PathBuf, Used> = u.into_iter().collect();
                for a in &mut self.apps {
                    a.used = m.get(&a.path).copied().unwrap_or(Used::Unknown);
                }
            }
            if s.done {
                self.scanning = false;
            }
        }
        let mut gone = vec![];
        if let Ok(mut d) = self.admin_done.try_lock() {
            for (app, paths) in d.drain(..) {
                self.admin_busy.remove(&app);
                gone.extend(paths);
            }
        }
        gone
    }

    fn prune(&mut self, removed: &HashSet<PathBuf>) {
        if removed.len() == self.seen_removed {
            return;
        }
        self.seen_removed = removed.len();
        self.apps.retain(|a| !removed.contains(&a.path));
        for a in &mut self.apps {
            a.leftovers.retain(|l| !removed.contains(&l.0));
        }
        self.orphans.retain(|o| !removed.contains(&o.path));
    }
}

// ---------- background scan ----------

fn names_in(dir: &Path) -> Vec<String> {
    fs::read_dir(dir)
        .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).collect())
        .unwrap_or_default()
}

/// .app bundles directly in each root, plus one level of plain subfolders (like Utilities).
fn find_apps(roots: &[PathBuf]) -> Vec<PathBuf> {
    let is_app = |p: &Path| p.extension().is_some_and(|e| e == "app");
    let real_dir = |p: &Path| fs::symlink_metadata(p).is_ok_and(|m| m.is_dir());
    let mut out = vec![];
    for root in roots {
        for n in names_in(root) {
            let p = root.join(&n);
            if n.starts_with('.') || !real_dir(&p) {
                continue;
            }
            if is_app(&p) {
                out.push(p);
            } else {
                out.extend(names_in(&p).into_iter().map(|c| p.join(c)).filter(|c| is_app(c) && real_dir(c)));
            }
        }
    }
    out
}

/// (bundle id, version) from Contents/Info.plist.
fn read_bundle(path: &Path) -> Option<(String, String)> {
    let v = plist::Value::from_file(path.join("Contents/Info.plist")).ok()?;
    let d = v.as_dictionary()?;
    let s = |k: &str| d.get(k).and_then(|v| v.as_string()).map(str::to_string);
    let id = s("CFBundleIdentifier").filter(|i| !i.is_empty())?;
    let ver = s("CFBundleShortVersionString").or_else(|| s("CFBundleVersion")).unwrap_or_default();
    Some((id, ver))
}

/// Bundle ids of helpers, extensions and login items shipped inside an app.
fn helper_ids(app: &Path) -> Vec<String> {
    let c = app.join("Contents");
    let dirs = ["Library/LoginItems", "Helpers", "Frameworks", "XPCServices", "PlugIns", "Library/LaunchServices", "MacOS"];
    dirs.iter()
        .flat_map(|d| {
            let dir = c.join(d);
            names_in(&dir).into_iter().map(move |n| dir.join(n))
        })
        .filter(|p| p.is_dir())
        .filter_map(|p| read_bundle(&p).map(|b| b.0))
        .collect()
}

fn my_groups() -> Vec<u32> {
    Command::new("/usr/bin/id")
        .arg("-G")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).split_whitespace().filter_map(|g| g.parse().ok()).collect())
        .unwrap_or_default()
}

/// Can `uid` (member of `groups`) write to a folder with this owner/group/mode?
fn writable(owner: u32, group: u32, mode: u32, uid: u32, groups: &[u32]) -> bool {
    if owner == uid {
        mode & 0o200 != 0
    } else if groups.contains(&group) {
        mode & 0o020 != 0
    } else {
        mode & 0o002 != 0
    }
}

fn scan_apps(shared: Arc<Mutex<Shared>>) {
    let home = scan::home();
    let lib = home.join("Library");
    let uid = fs::metadata(&home).map(|m| m.uid()).unwrap_or(u32::MAX);
    let groups = my_groups();

    let mut apps = vec![];
    // Everything that counts as "installed" for the orphan check.
    let mut known: Vec<String> = vec![];
    for path in find_apps(&[PathBuf::from("/Applications"), home.join("Applications")]) {
        let Some((id, version)) = read_bundle(&path) else { continue };
        known.push(id.clone());
        known.extend(helper_ids(&path));
        let Ok(meta) = fs::metadata(&path) else { continue };
        // Apple apps, and anything that really lives on the sealed system volume.
        let sealed = fs::canonicalize(&path).is_ok_and(|p| p.starts_with("/System"));
        if id.to_ascii_lowercase().starts_with("com.apple.") || sealed {
            continue;
        }
        let parent_ok = path
            .parent()
            .and_then(|p| fs::metadata(p).ok())
            .is_some_and(|m| writable(m.uid(), m.gid(), m.mode(), uid, &groups));
        let name = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        apps.push(AppInfo {
            size: scan::dir_size(&path),
            path,
            name,
            id,
            version,
            leftovers: vec![],
            locked: meta.uid() != uid || !parent_ok,
            used: Used::Loading,
        });
    }

    // Leftovers: each ~/Library folder is listed once, every entry goes to its most specific owner.
    let owners: Vec<(&str, &str)> = apps.iter().map(|a| (a.id.as_str(), a.name.as_str())).collect();
    let mut found: Vec<Vec<PathBuf>> = vec![vec![]; apps.len()];
    for dir in LIB_DIRS {
        for entry in names_in(&lib.join(dir)) {
            if let Some(i) = owner(dir, &entry, &owners) {
                found[i].push(lib.join(dir).join(entry));
            }
        }
    }
    for (a, paths) in apps.iter_mut().zip(found) {
        a.leftovers = paths.into_iter().map(|p| (scan::dir_size(&p), p)).map(|(s, p)| (p, s)).collect();
        a.leftovers.sort_by(|x, y| y.1.cmp(&x.1));
    }
    apps.sort_by(|a, b| b.total().cmp(&a.total()));
    let paths: Vec<PathBuf> = apps.iter().map(|a| a.path.clone()).collect();
    if let Ok(mut s) = shared.lock() {
        s.apps = Some(apps);
    }

    // Leftovers of deleted apps.
    for d in ["/Library/LaunchAgents", "/Library/LaunchDaemons", "/Library/PrivilegedHelperTools"] {
        known.extend(names_in(Path::new(d)).into_iter().map(|n| n.trim_end_matches(".plist").to_string()));
    }
    known.extend(names_in(&lib.join("LaunchAgents")).into_iter().map(|n| n.trim_end_matches(".plist").to_string()));
    let orphans = find_orphans(&lib, &known);
    if let Ok(mut s) = shared.lock() {
        s.orphans = Some(orphans);
    }

    // Last used dates (slowest part, so it comes last).
    let mut used: Vec<(PathBuf, Used)> = paths
        .into_iter()
        .map(|p| {
            let out = Command::new("/usr/bin/mdls")
                .args(["-raw", "-name", "kMDItemLastUsedDate"])
                .arg(&p)
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
                .unwrap_or_default();
            let u = parse_date(&out).map_or(Used::Never, Used::At);
            (p, u)
        })
        .collect();
    if used.iter().all(|u| u.1 == Used::Never) {
        for u in &mut used {
            u.1 = Used::Unknown;
        }
    }
    if let Ok(mut s) = shared.lock() {
        s.used = Some(used);
        s.done = true;
    }
}

fn find_orphans(lib: &Path, known: &[String]) -> Vec<Orphan> {
    let recent = SystemTime::now() - Duration::from_secs(14 * DAY as u64);
    let mut installed: HashMap<String, bool> = HashMap::new();
    let mut out = vec![];
    for dir in ORPHAN_DIRS {
        for entry in names_in(&lib.join(dir)) {
            let Some(id) = orphan_id(&entry, known) else { continue };
            let path = lib.join(dir).join(&entry);
            let Ok(m) = fs::symlink_metadata(&path) else { continue };
            // Recently touched means something still uses it.
            if m.file_type().is_symlink() || m.modified().map_or(true, |t| t > recent) {
                continue;
            }
            let size = scan::dir_size(&path);
            if size < MB {
                continue;
            }
            // Last check: is an app with this id anywhere on the Mac (Spotlight)?
            let present = *installed.entry(id.to_ascii_lowercase()).or_insert_with(|| {
                Command::new("/usr/bin/mdfind")
                    .arg(format!("kMDItemCFBundleIdentifier == '{id}'c"))
                    .output()
                    .map_or(true, |o| !o.stdout.iter().all(|b| b.is_ascii_whitespace()))
            });
            if !present {
                out.push(Orphan { path, id, size, selected: false });
            }
        }
    }
    out.sort_by(|a, b| b.size.cmp(&a.size));
    out
}

// ---------- matching (pure) ----------

/// `entry` is `id` itself (ignoring case) or something under it (`id.` + more, exact case,
/// so "com.google.chrome.for.testing" isn't taken for part of "com.google.Chrome").
fn id_match(entry: &str, id: &str) -> bool {
    !id.is_empty() && (entry.eq_ignore_ascii_case(id) || entry.strip_prefix(id).is_some_and(|r| r.starts_with('.')))
}

/// How strongly `entry` in ~/Library/`dir` belongs to the app: Some(id len + 1) for a
/// bundle-id match, Some(0) for a match on the app's name only, None if it doesn't.
fn match_score(dir: &str, entry: &str, id: &str, name: &str) -> Option<usize> {
    let by_id = Some(id.len() + 1);
    let eq = |a: &str, b: &str| !b.is_empty() && a.eq_ignore_ascii_case(b);
    let hit = |ok: bool, score: Option<usize>| if ok { score } else { None };
    match dir {
        "Application Support" | "Logs" => {
            if eq(entry, id) {
                by_id
            } else {
                hit(name.len() >= 3 && eq(entry, name), Some(0))
            }
        }
        "Caches" | "Containers" | "HTTPStorages" | "WebKit" | "Application Scripts" => hit(id_match(entry, id), by_id),
        "Preferences" | "LaunchAgents" => hit(entry.strip_suffix(".plist").is_some_and(|s| id_match(s, id)), by_id),
        "Saved Application State" => hit(entry.strip_suffix(".savedState").is_some_and(|s| eq(s, id)), by_id),
        "Cookies" => hit(entry.strip_suffix(".binarycookies").is_some_and(|s| eq(s, id)), by_id),
        "Group Containers" => {
            let team = entry.split_once('.').filter(|(t, _)| {
                t.len() == 10 && t.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
            });
            hit(team.is_some_and(|(_, rest)| eq(rest, id)), by_id)
        }
        _ => None,
    }
}

/// Index of the app that owns this entry: the single best match. Ties (e.g. two copies of
/// one app, or two apps with the same name) belong to nobody, so shared data is never removed.
fn owner(dir: &str, entry: &str, apps: &[(&str, &str)]) -> Option<usize> {
    let scores: Vec<(usize, usize)> =
        apps.iter().enumerate().filter_map(|(i, (id, name))| match_score(dir, entry, id, name).map(|s| (i, s))).collect();
    let best = scores.iter().map(|s| s.1).max()?;
    let mut top = scores.iter().filter(|s| s.1 == best);
    let first = top.next()?.0;
    top.next().is_none().then_some(first)
}

/// The bundle id if this folder name looks like one (reverse-DNS, 3+ parts).
fn bundle_like(entry: &str) -> Option<&str> {
    let id = entry.strip_suffix(".savedState").or_else(|| entry.strip_suffix(".binarycookies")).unwrap_or(entry);
    let parts: Vec<&str> = id.split('.').collect();
    let tld = parts[0];
    let ok = parts.len() >= 3
        && (2..=12).contains(&tld.len())
        && tld.bytes().all(|b| b.is_ascii_lowercase())
        && parts.iter().all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'));
    ok.then_some(id)
}

/// First two parts, lowercased: "com.google.Chrome" -> "com.google".
fn vendor(id: &str) -> String {
    id.split('.').take(2).collect::<Vec<_>>().join(".").to_ascii_lowercase()
}

/// Some(bundle id) when `entry` looks like data of an app that isn't installed. Conservative:
/// anything sharing a vendor (first two parts) with an installed app or helper is kept.
fn orphan_id(entry: &str, known: &[String]) -> Option<String> {
    let id = bundle_like(entry)?;
    let lower = id.to_ascii_lowercase();
    // Command-line tools (com.vercel.cli, …) keep caches without any app bundle.
    if NEVER_ORPHAN.iter().any(|p| lower.starts_with(p)) || lower.ends_with(".cli") {
        return None;
    }
    let v = vendor(id);
    if known.iter().any(|k| vendor(k) == v || id_match(id, k) || id_match(k, id)) {
        return None;
    }
    Some(id.to_string())
}

// ---------- dates ----------

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Parses mdls output like "2026-05-27 03:34:56 +0000" into unix seconds. "(null)" -> None.
fn parse_date(s: &str) -> Option<i64> {
    let mut it = s.trim().split_whitespace();
    let nums = |s: &str, sep: char| -> Option<Vec<i64>> {
        let v: Vec<i64> = s.split(sep).map(|x| x.parse().ok()).collect::<Option<_>>()?;
        (v.len() == 3).then_some(v)
    };
    let d = nums(it.next()?, '-')?;
    let t = nums(it.next()?, ':')?;
    let tz = it.next().unwrap_or("+0000");
    let sign = if tz.starts_with('-') { -1 } else { 1 };
    let z: i64 = tz.get(1..)?.parse().ok()?;
    let offset = sign * (z / 100 * 3600 + z % 100 * 60);
    Some(days_from_civil(d[0], d[1], d[2]) * DAY + t[0] * 3600 + t[1] * 60 + t[2] - offset)
}

fn plural(n: i64, unit: &str) -> String {
    if n == 1 { format!("1 {unit}") } else { format!("{n} {unit}s") }
}

fn span(days: i64) -> String {
    match days {
        d if d < 14 => plural(d, "day"),
        d if d < 60 => plural(d / 7, "week"),
        d if d < 365 => plural(d / 30, "month"),
        d => plural(d / 365, "year"),
    }
}

fn used_label(u: Used, now: i64) -> String {
    match u {
        Used::Loading => "Checking last use…".into(),
        Used::Unknown => String::new(),
        Used::Never => "Never opened".into(),
        Used::At(t) => match (now - t).max(0) / DAY {
            0 => "Used today".into(),
            1 => "Used yesterday".into(),
            d if d >= 90 => format!("Not opened in {}", span(d)),
            d => format!("Used {} ago", span(d)),
        },
    }
}

fn unused(u: Used, now: i64) -> bool {
    match u {
        Used::Never => true,
        Used::At(t) => now - t >= 90 * DAY,
        _ => false,
    }
}

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

// ---------- actions ----------

fn regex_escape(s: &str) -> String {
    s.chars().fold(String::new(), |mut o, c| {
        if "\\^$.|?*+()[]{}".contains(c) {
            o.push('\\');
        }
        o.push(c);
        o
    })
}

fn is_running(app: &Path) -> bool {
    let pat = format!("{}/Contents/MacOS/", regex_escape(&app.to_string_lossy()));
    Command::new("/usr/bin/pgrep").arg("-f").arg(pat).output().is_ok_and(|o| o.status.success())
}

/// Moves a root-owned app to the Trash with the password prompt, then trashes its leftovers.
fn admin_uninstall(
    app: PathBuf,
    name: String,
    leftovers: Vec<PathBuf>,
    done: Arc<Mutex<Vec<(PathBuf, Vec<PathBuf>)>>>,
    tx: Sender<Msg>,
) {
    let stem = app.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let dest = scan::unique(&scan::home().join(".Trash"), &stem, "app");
    let (a, d) = (app.to_string_lossy().into_owned(), dest.to_string_lossy().into_owned());
    let mut gone = vec![];
    let msg = if [&a, &d].iter().any(|s| s.contains(['\'', '"', '\\'])) {
        format!("Can't uninstall {name}: unusual characters in its path. Drag it to the Trash in Finder")
    } else {
        match system::admin(&format!("/bin/mv '{a}' '{d}'")) {
            Err(e) if e.contains("-128") => "Uninstall cancelled".to_string(),
            Err(e) => format!("Couldn't uninstall {name}: {e}"),
            Ok(()) => {
                gone.push(app.clone());
                let mut failed = 0;
                for l in leftovers {
                    match scan::remove(&l, false) {
                        Ok(()) => gone.push(l),
                        Err(_) => failed += 1,
                    }
                }
                if failed == 0 {
                    format!("{name} moved to Trash")
                } else {
                    format!("{name} moved to Trash · {failed} leftover(s) couldn't be removed")
                }
            }
        }
    };
    if let Ok(mut d) = done.lock() {
        d.push((app, gone));
    }
    let _ = tx.send(Msg::Status(msg));
}

// ---------- UI ----------

/// A path line inside an expanded app: path, size, Show in Finder. Returns reveal click.
fn path_row(ui: &mut Ui, p: &Path, size: u64, label: Option<&str>) -> bool {
    let mut reveal = false;
    ui.allocate_ui_with_layout(vec2(ui.available_width(), 24.0), Layout::right_to_left(Align::Center), |ui| {
        reveal = icon_button(ui, ic::FOLDER_OPEN, DIM, "Show in Finder").clicked();
        ui.add_sized(vec2(84.0, 18.0), Label::new(RichText::new(human(size)).small().color(Color32::WHITE)));
        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
            ui.add_space(52.0);
            let text = match label {
                Some(l) => format!("{l}  ·  {}", short(p)),
                None => short(p),
            };
            ui.add(Label::new(RichText::new(text).small().color(DIM)).truncate());
        });
    });
    reveal
}

enum Act {
    Toggle(PathBuf),
    Uninstall(usize),
    Reveal(PathBuf),
}

impl App {
    pub(crate) fn uninstall_page(&mut self, ui: &mut Ui) {
        if !self.uninstall.started {
            self.uninstall.rescan();
        }
        let gone = self.uninstall.poll();
        self.removed.extend(gone);
        self.uninstall.prune(&self.removed);
        if self.uninstall.scanning || !self.uninstall.admin_busy.is_empty() {
            ui.ctx().request_repaint_after(Duration::from_millis(250));
        }

        let scanning = self.uninstall.scanning;
        let mut rescan = false;
        header(
            ui,
            ic::PACKAGE,
            ACCENT,
            "App Uninstaller",
            "Remove apps completely, along with the files they leave behind.",
            |ui| {
                let b = Button::new(format!("{}  Rescan", ic::ARROWS_CLOCKWISE)).rounding(9.0).min_size(vec2(0.0, 34.0));
                rescan = ui.add_enabled(!scanning, b).clicked();
            },
        );
        if rescan {
            self.uninstall.rescan();
        }
        if scanning {
            ui.label(RichText::new(if self.uninstall.apps.is_empty() { "Looking at your apps…" } else { "Checking leftovers and last use…" }).small().color(DIM));
            indeterminate(ui, ACCENT);
            ui.add_space(10.0);
        }

        // Toolbar
        let now = now();
        let st = &mut self.uninstall;
        let q = st.query.to_lowercase();
        let mut ids: Vec<usize> = (0..st.apps.len())
            .filter(|&i| {
                let a = &st.apps[i];
                (q.is_empty() || a.name.to_lowercase().contains(&q) || a.id.to_lowercase().contains(&q))
                    && (st.filter != Filter::Unused || unused(a.used, now))
            })
            .collect();
        if st.filter == Filter::Largest {
            ids.truncate(LARGEST);
        }
        let shown: u64 = ids.iter().map(|&i| st.apps[i].total()).sum();
        ui.horizontal(|ui| {
            search_field(ui, &mut st.query, "Search apps", 240.0);
            ui.add_space(6.0);
            for (f, t) in [(Filter::All, "All"), (Filter::Unused, "Unused 90+ days"), (Filter::Largest, "Largest")] {
                if chip(ui, st.filter == f, t) {
                    st.filter = f;
                }
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(RichText::new(format!("{} apps · {}", ids.len(), human(shown))).color(DIM));
            });
        });
        ui.add_space(12.0);

        let mut act = None;
        let mut remove_orphans = false;
        let busy_del = self.del_total > 0;
        let deleting = &self.deleting;
        let thumbs = &mut self.thumbs;
        egui::ScrollArea::vertical().id_salt("uninstall").auto_shrink([false; 2]).show(ui, |ui| {
            card(ui, |ui| {
                if ids.is_empty() {
                    ui.add_space(24.0);
                    ui.vertical_centered(|ui| {
                        let t = if scanning {
                            "Looking…"
                        } else if st.filter == Filter::Unused && st.apps.iter().any(|a| a.used == Used::Unknown) {
                            "Spotlight has no usage data on this Mac"
                        } else {
                            "No apps here"
                        };
                        ui.label(RichText::new(t).color(DIM));
                    });
                    ui.add_space(24.0);
                }
                for (k, &i) in ids.iter().enumerate() {
                    let a = &st.apps[i];
                    let open = st.open.contains(&a.path);
                    let busy = st.admin_busy.contains(&a.path) || deleting.contains(&a.path);
                    let row_rect = Rect::from_min_size(ui.cursor().min, vec2(ui.available_width(), 56.0));
                    let row = ui.interact(row_rect, Id::new(("app-row", &a.path)), Sense::click());
                    if ui.rect_contains_pointer(row_rect) {
                        ui.painter().rect_filled(row_rect.expand2(vec2(6.0, 0.0)), 10.0, Color32::from_white_alpha(7));
                    }
                    if row.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                        act = Some(Act::Toggle(a.path.clone()));
                    }
                    ui.allocate_ui_with_layout(vec2(ui.available_width(), 56.0), Layout::right_to_left(Align::Center), |ui| {
                        if busy {
                            ui.add(egui::Spinner::new().size(16.0).color(DANGER));
                            ui.label(RichText::new("Removing…").small().color(DIM));
                        } else if soft(ui, !busy_del, format!("{}  Uninstall", ic::TRASH), DANGER).clicked() {
                            act = Some(Act::Uninstall(i));
                        }
                        ui.add_sized(vec2(84.0, 20.0), Label::new(RichText::new(human(a.total())).strong().color(Color32::WHITE)));
                        let chev = if open { ic::CARET_DOWN } else { ic::CARET_RIGHT };
                        if icon_button(ui, chev, DIM, "Show files").clicked() {
                            act = Some(Act::Toggle(a.path.clone()));
                        }
                        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                            thumbs::preview(ui, thumbs, &a.path, 38.0, ic::PACKAGE, ACCENT);
                            ui.add_space(4.0);
                            ui.vertical(|ui| {
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new(&a.name).strong().color(Color32::WHITE));
                                    if !a.version.is_empty() {
                                        ui.label(RichText::new(&a.version).small().color(DIM));
                                    }
                                    if a.locked {
                                        ui.label(RichText::new(ic::LOCK).small().color(WARN))
                                            .on_hover_text("Installed for all users. Removing it needs your password");
                                    }
                                });
                                let used = used_label(a.used, now);
                                let sub = if used.is_empty() { a.id.clone() } else { format!("{} · {used}", a.id) };
                                let c = if unused(a.used, now) { WARN } else { DIM };
                                ui.add(Label::new(RichText::new(sub).small().color(c)).truncate());
                            });
                        });
                    });
                    if open {
                        if path_row(ui, &a.path, a.size, Some("App")) {
                            act = Some(Act::Reveal(a.path.clone()));
                        }
                        for (p, s) in &a.leftovers {
                            if path_row(ui, p, *s, None) {
                                act = Some(Act::Reveal(p.clone()));
                            }
                        }
                        if a.leftovers.is_empty() {
                            ui.horizontal(|ui| {
                                ui.add_space(52.0);
                                ui.label(RichText::new("No leftover files found").small().color(DIM));
                            });
                        }
                    }
                    if k + 1 < ids.len() {
                        ui.separator();
                    }
                }
            });
            ui.add_space(14.0);

            // Leftovers from deleted apps
            card(ui, |ui| {
                let sel: u64 = st.orphans.iter().filter(|o| o.selected).map(|o| o.size).sum();
                let n_sel = st.orphans.iter().filter(|o| o.selected).count();
                let all = !st.orphans.is_empty() && st.orphans.iter().all(|o| o.selected);
                ui.horizontal(|ui| {
                    badge(ui, ic::GHOST, WARN, 34.0);
                    ui.vertical(|ui| {
                        ui.label(RichText::new("Leftovers from deleted apps").strong().color(Color32::WHITE));
                        let total: u64 = st.orphans.iter().map(|o| o.size).sum();
                        ui.label(RichText::new(format!("{} items · {}", st.orphans.len(), human(total))).small().color(DIM));
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let t = if n_sel == 0 {
                            format!("{}  Remove selected", ic::TRASH)
                        } else {
                            format!("{}  Remove {n_sel} · {}", ic::TRASH, human(sel))
                        };
                        remove_orphans = primary(ui, n_sel > 0 && !busy_del, t, DANGER).clicked();
                        if !st.orphans.is_empty() {
                            let t = if all { format!("{} Select none", ic::SQUARE) } else { format!("{} Select all", ic::CHECK_SQUARE) };
                            if ui.button(t).clicked() {
                                st.orphans.iter_mut().for_each(|o| o.selected = !all);
                            }
                        }
                    });
                });
                ui.add_space(8.0);
                if st.orphans.is_empty() {
                    let t = if scanning { "Looking…" } else { "Nothing left behind by deleted apps" };
                    ui.label(RichText::new(t).color(DIM));
                }
                let n = st.orphans.len();
                for (k, o) in st.orphans.iter_mut().enumerate() {
                    let busy = deleting.contains(&o.path);
                    ui.allocate_ui_with_layout(vec2(ui.available_width(), 44.0), Layout::right_to_left(Align::Center), |ui| {
                        if busy {
                            ui.add(egui::Spinner::new().size(16.0).color(DANGER));
                        } else if icon_button(ui, ic::FOLDER_OPEN, DIM, "Show in Finder").clicked() {
                            act = Some(Act::Reveal(o.path.clone()));
                        }
                        ui.add_sized(vec2(84.0, 20.0), Label::new(RichText::new(human(o.size)).strong().color(Color32::WHITE)));
                        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                            ui.add_enabled(!busy, egui::Checkbox::without_text(&mut o.selected));
                            ui.vertical(|ui| {
                                ui.add(Label::new(RichText::new(&o.id).color(Color32::WHITE)).truncate());
                                ui.add(Label::new(RichText::new(short(&o.path)).small().color(DIM)).truncate());
                            });
                        });
                    });
                    if k + 1 < n {
                        ui.separator();
                    }
                }
            });
        });

        match act {
            Some(Act::Toggle(p)) => {
                if !self.uninstall.open.remove(&p) {
                    self.uninstall.open.insert(p);
                }
            }
            Some(Act::Reveal(p)) => system::reveal(&p),
            Some(Act::Uninstall(i)) => {
                let a = &self.uninstall.apps[i];
                if is_running(&a.path) {
                    let msg = format!("Quit {} first", a.name);
                    self.toast(msg, false);
                } else if a.locked {
                    self.uninstall.admin_ask = Some(a.path.clone());
                } else {
                    let title = format!("Uninstall {}?", a.name);
                    let mut entries = vec![(a.path.clone(), format!("{} (app)", a.name), a.size)];
                    entries.extend(a.leftovers.iter().map(|(p, s)| (p.clone(), short(p), *s)));
                    self.ask_delete_paths(title, entries, false);
                }
            }
            None => {}
        }
        if remove_orphans {
            let entries = self.uninstall.orphans.iter().filter(|o| o.selected).map(|o| (o.path.clone(), short(&o.path), o.size)).collect();
            self.ask_delete_paths("Remove leftovers of deleted apps?", entries, false);
        }
        self.admin_modal(ui.ctx());
    }

    /// Confirmation for apps that need the admin password.
    fn admin_modal(&mut self, ctx: &egui::Context) {
        let Some(path) = self.uninstall.admin_ask.clone() else { return };
        let Some(a) = self.uninstall.apps.iter().find(|a| a.path == path) else {
            self.uninstall.admin_ask = None;
            return;
        };
        egui::Area::new(Id::new("uninstall-dim")).order(Order::Middle).fixed_pos(pos2(0.0, 0.0)).show(ctx, |ui| {
            let screen = ctx.screen_rect();
            ui.painter().rect_filled(screen, 0.0, Color32::from_black_alpha(170));
            ui.allocate_rect(screen, Sense::click());
        });
        let (mut cancel, mut go) = (ctx.input(|i| i.key_pressed(egui::Key::Escape)), false);
        egui::Area::new(Id::new("uninstall-admin")).order(Order::Foreground).anchor(Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
            Frame::none().fill(CARD).rounding(16.0).stroke(Stroke::new(1.0_f32, BORDER)).inner_margin(24.0).show(ui, |ui| {
                ui.set_width(400.0);
                badge(ui, ic::LOCK, WARN, 44.0);
                ui.add_space(8.0);
                ui.label(RichText::new(format!("Uninstall {}?", a.name)).size(18.0).strong().color(Color32::WHITE));
                ui.label(RichText::new(format!("{} item(s) · {}", a.leftovers.len() + 1, human(a.total()))).color(DIM));
                ui.add_space(8.0);
                ui.label(
                    RichText::new("This app was installed for all users, so macOS will ask for your password to move it to the Trash. Its leftover files go to the Trash too.")
                        .small()
                        .color(DIM),
                );
                ui.add_space(16.0);
                ui.horizontal(|ui| {
                    go = primary(ui, true, format!("{}  Move to Trash", ic::TRASH), DANGER).clicked();
                    if ui.add(Button::new("Cancel").rounding(9.0).min_size(vec2(80.0, 34.0))).clicked() {
                        cancel = true;
                    }
                });
            });
        });
        if go {
            let (name, leftovers) = (a.name.clone(), a.leftovers.iter().map(|l| l.0.clone()).collect());
            let (done, tx) = (self.uninstall.admin_done.clone(), self.tx.clone());
            self.uninstall.admin_busy.insert(path.clone());
            self.uninstall.admin_ask = None;
            std::thread::spawn(move || admin_uninstall(path, name, leftovers, done, tx));
        } else if cancel {
            self.uninstall.admin_ask = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_prefix_needs_dot_boundary() {
        assert!(id_match("com.foo.app", "com.foo.app"));
        assert!(id_match("com.foo.app.ShipIt", "com.foo.app"));
        assert!(id_match("COM.Foo.App", "com.foo.app"));
        assert!(!id_match("com.google.chrome.for.testing", "com.google.Chrome"));
        assert!(!id_match("com.foo.application", "com.foo.app"));
        assert!(!id_match("com.foo", "com.foo.app"));
        assert!(!id_match("anything", ""));
    }

    #[test]
    fn leftover_rules() {
        let (id, name) = ("com.tinyspeck.slackmacgap", "Slack");
        let m = |d: &str, e: &str| match_score(d, e, id, name).is_some();
        assert!(m("Application Support", "Slack"));
        assert!(m("Application Support", "com.tinyspeck.slackmacgap"));
        assert!(!m("Application Support", "Slack Helper"));
        assert!(m("Caches", "com.tinyspeck.slackmacgap.ShipIt"));
        assert!(!m("Caches", "Slack"), "caches match by id only");
        assert!(m("Preferences", "com.tinyspeck.slackmacgap.plist"));
        assert!(m("Preferences", "com.tinyspeck.slackmacgap.helper.plist"));
        assert!(!m("Preferences", "com.tinyspeck.slackmacgapX.plist"));
        assert!(m("Containers", "com.tinyspeck.slackmacgap"));
        assert!(m("Group Containers", "BQR82RBBHL.com.tinyspeck.slackmacgap"));
        assert!(!m("Group Containers", "BQR82RBBHL.com.tinyspeck.slackmacgap.other"));
        assert!(!m("Group Containers", "group.com.tinyspeck.slackmacgap"));
        assert!(!m("Group Containers", "bqr82rbbhl.com.tinyspeck.slackmacgap"));
        assert!(m("Saved Application State", "com.tinyspeck.slackmacgap.savedState"));
        assert!(!m("Saved Application State", "com.tinyspeck.slackmacgap"));
        assert!(m("Logs", "Slack"));
        assert!(m("HTTPStorages", "com.tinyspeck.slackmacgap.binarycookies"));
        assert!(m("WebKit", "com.tinyspeck.slackmacgap"));
        assert!(m("Application Scripts", "com.tinyspeck.slackmacgap"));
        assert!(m("LaunchAgents", "com.tinyspeck.slackmacgap.updater.plist"));
        assert!(m("Cookies", "com.tinyspeck.slackmacgap.binarycookies"));
        assert!(!m("Desktop", "com.tinyspeck.slackmacgap"));
        // Very short names never match by name.
        assert!(match_score("Application Support", "Go", "com.x.go", "Go").is_none());
    }

    #[test]
    fn most_specific_owner_wins() {
        let apps = [("com.google.Chrome", "Google Chrome"), ("com.google.Chrome.canary", "Google Chrome Canary")];
        assert_eq!(owner("Caches", "com.google.Chrome", &apps), Some(0));
        assert_eq!(owner("Caches", "com.google.Chrome.canary", &apps), Some(1));
        assert_eq!(owner("Caches", "com.google.Chrome.canary.helper", &apps), Some(1));
        assert_eq!(owner("Caches", "com.google.Chrome.helper", &apps), Some(0));
        // Two copies of one app: shared data belongs to neither.
        let dup = [("com.foo.app", "Foo"), ("com.foo.app", "Foo 2")];
        assert_eq!(owner("Containers", "com.foo.app", &dup), None);
        assert_eq!(owner("Application Support", "Foo", &dup), Some(0));
        // Bundle id beats a name match.
        let mixed = [("com.a.x", "Thing"), ("Thing", "Other")];
        assert_eq!(owner("Application Support", "Thing", &mixed), Some(1));
    }

    #[test]
    fn orphan_matcher() {
        let known = vec!["com.google.Chrome".to_string(), "com.tinyspeck.slackmacgap".to_string(), "org.mozilla.firefox".into()];
        assert_eq!(orphan_id("com.spotify.client", &known).as_deref(), Some("com.spotify.client"));
        assert_eq!(orphan_id("com.spotify.client.savedState", &known).as_deref(), Some("com.spotify.client"));
        // Installed app, its helpers and same-vendor items are kept.
        assert_eq!(orphan_id("com.google.Chrome", &known), None);
        assert_eq!(orphan_id("com.google.Chrome.helper", &known), None);
        assert_eq!(orphan_id("com.google.Keystone", &known), None);
        assert_eq!(orphan_id("COM.TINYSPECK.slackmacgap.ShipIt", &known), None, "same vendor, any case");
        // System and shared frameworks.
        assert_eq!(orphan_id("com.apple.Safari", &known), None);
        assert_eq!(orphan_id("group.com.apple.notes", &known), None);
        assert_eq!(orphan_id("systemgroup.com.apple.foo", &known), None);
        assert_eq!(orphan_id("org.sparkle-project.Sparkle", &known), None);
        assert_eq!(orphan_id("com.vercel.cli", &known), None);
        assert_eq!(orphan_id("com.github.Electron.savedState", &known), None);
        // Not bundle ids.
        assert_eq!(orphan_id("Slack", &known), None);
        assert_eq!(orphan_id("com.spotify", &known), None);
        assert_eq!(orphan_id("Google Chrome.app", &known), None);
        assert_eq!(orphan_id("1.2.3", &known), None);
        assert_eq!(orphan_id("Docker Desktop", &known), None);
        assert_eq!(orphan_id("com..foo", &known), None);
        assert_eq!(orphan_id("60723E4F-4C1C-4931-A360-F085B089F3C6", &known), None);
        assert_eq!(orphan_id("com.foo bar.baz", &known), None);
    }

    #[test]
    fn dates_and_labels() {
        assert_eq!(parse_date("1970-01-01 00:00:00 +0000"), Some(0));
        assert_eq!(parse_date("2000-03-01 00:00:00 +0000"), Some(951_868_800));
        assert_eq!(parse_date("2026-05-27 03:34:56 +0000\n"), Some(1_779_852_896));
        assert_eq!(parse_date("1970-01-01 01:00:00 +0100"), Some(0));
        assert_eq!(parse_date("(null)"), None);
        assert_eq!(parse_date(""), None);
        let now = 1_000 * DAY;
        assert_eq!(used_label(Used::At(now - 3 * DAY), now), "Used 3 days ago");
        assert_eq!(used_label(Used::At(now - 1000), now), "Used today");
        assert_eq!(used_label(Used::At(now - DAY - 5), now), "Used yesterday");
        assert_eq!(used_label(Used::At(now - 21 * DAY), now), "Used 3 weeks ago");
        assert_eq!(used_label(Used::At(now - 215 * DAY), now), "Not opened in 7 months");
        assert_eq!(used_label(Used::At(now - 400 * DAY), now), "Not opened in 1 year");
        assert_eq!(used_label(Used::Never, now), "Never opened");
        assert!(unused(Used::Never, now) && unused(Used::At(now - 90 * DAY), now));
        assert!(!unused(Used::At(now - 89 * DAY), now) && !unused(Used::Unknown, now));
    }

    #[test]
    fn write_permission() {
        let groups = [20, 80];
        assert!(writable(501, 0, 0o755, 501, &groups));
        assert!(writable(0, 80, 0o775, 501, &groups), "/Applications for admins");
        assert!(!writable(0, 0, 0o755, 501, &groups));
        assert!(!writable(0, 12, 0o775, 501, &groups));
        assert!(writable(0, 0, 0o777, 501, &groups));
    }

    #[test]
    fn finds_apps_and_leftovers_in_temp_dirs() {
        let base = std::env::temp_dir().join(format!("cleanyou-uninstall-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let mk = |p: &Path, id: &str| {
            fs::create_dir_all(p.join("Contents")).unwrap();
            let mut d = plist::Dictionary::new();
            d.insert("CFBundleIdentifier".into(), id.into());
            d.insert("CFBundleShortVersionString".into(), "1.2".into());
            plist::Value::Dictionary(d).to_file_xml(p.join("Contents/Info.plist")).unwrap();
        };
        mk(&base.join("Apps/Foo.app"), "com.example.foo");
        mk(&base.join("Apps/Utilities/Bar.app"), "com.example.bar");
        mk(&base.join("Apps/Foo.app/Contents/Library/LoginItems/FooHelper.app"), "net.other.foohelper");
        fs::create_dir_all(base.join("Apps/Utilities/Deep/Nope.app")).unwrap();
        fs::create_dir_all(base.join("Apps/.hidden/Hidden.app")).unwrap();
        let mut found = find_apps(&[base.join("Apps")]);
        found.sort();
        assert_eq!(found, vec![base.join("Apps/Foo.app"), base.join("Apps/Utilities/Bar.app")]);
        assert_eq!(read_bundle(&found[0]), Some(("com.example.foo".into(), "1.2".into())));
        assert_eq!(helper_ids(&found[0]), vec!["net.other.foohelper".to_string()]);
        assert!(read_bundle(&base.join("Apps/Utilities/Deep/Nope.app")).is_none());

        // Orphan scan over a fake ~/Library (old enough, big enough).
        let lib = base.join("Library");
        for (dir, name, size) in [
            ("Caches", "com.gone.app", 2_000_000),
            ("Caches", "com.gone.small", 10_000),
            ("Caches", "com.example.foo", 2_000_000),
            ("Containers", "net.other.foohelper", 2_000_000),
        ] {
            let p = lib.join(dir).join(name);
            fs::create_dir_all(&p).unwrap();
            fs::write(p.join("data"), vec![1u8; size]).unwrap();
            let old = SystemTime::now() - Duration::from_secs(60 * DAY as u64);
            fs::File::open(&p).unwrap().set_modified(old).unwrap();
        }
        let known = vec!["com.example.foo".to_string(), "net.other.foohelper".to_string()];
        let orphans = find_orphans(&lib, &known);
        let ids: Vec<&str> = orphans.iter().map(|o| o.id.as_str()).collect();
        assert_eq!(ids, vec!["com.gone.app"]);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn regex_is_escaped() {
        assert_eq!(regex_escape("/Applications/T3 Code (Alpha).app"), "/Applications/T3 Code \\(Alpha\\)\\.app");
    }
}
