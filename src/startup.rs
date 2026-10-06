//! Startup Items: login items, launch agents and daemons.

use super::*;
use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

const LOGIN_SETTINGS: &str = "x-apple.systempreferences:com.apple.LoginItems-Settings.extension";

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Scope {
    /// ~/Library/LaunchAgents
    User,
    /// /Library/LaunchAgents (runs for every user, gui domain)
    AllUsers,
    /// /Library/LaunchDaemons (root, needs a password)
    System,
}

#[derive(Clone, Debug)]
pub struct Job {
    pub label: String,
    pub plist: PathBuf,
    pub program: String,
    pub app: Option<String>,
    pub scope: Scope,
    pub run_at_load: bool,
    pub keep_alive: bool,
    pub enabled: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LoginItem {
    pub name: String,
    pub path: Option<String>,
    pub on: bool,
}

#[derive(Default)]
struct Snapshot {
    login: Vec<LoginItem>,
    login_err: Option<String>,
    jobs: Vec<Job>,
    uid: u32,
}

#[derive(Default)]
pub struct State {
    last_frame: Option<u64>,
    loading: bool,
    incoming: Arc<Mutex<Option<Snapshot>>>,
    snap: Snapshot,
    /// Login items removed this session, so they can be switched back on.
    removed_login: Vec<LoginItem>,
    busy: HashSet<String>,
}

// ---------- pure helpers ----------

/// Parses `launchctl print-disabled` output into label -> disabled.
pub fn parse_disabled(text: &str) -> HashMap<String, bool> {
    let mut map = HashMap::new();
    for line in text.lines() {
        let Some((k, v)) = line.split_once("=>") else { continue };
        let k = k.trim().trim_matches('"');
        if k.is_empty() {
            continue;
        }
        let disabled = match v.trim().trim_end_matches(',') {
            "disabled" | "true" => true,
            "enabled" | "false" => false,
            _ => continue,
        };
        map.insert(k.to_string(), disabled);
    }
    map
}

/// AppleScript list output ("A, B, C") into items.
pub fn parse_as_list(text: &str) -> Vec<String> {
    let t = text.trim();
    if t.is_empty() {
        return vec![];
    }
    t.split(", ").map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
}

/// "/Applications/Docker.app/Contents/MacOS/x" -> "Docker".
pub fn app_from_path(p: &str) -> Option<String> {
    p.split('/').find(|c| c.ends_with(".app") && c.len() > 4).map(|c| c.trim_end_matches(".app").to_string())
}

fn cap(w: &str) -> String {
    let mut c = w.chars();
    match c.next() {
        Some(f) if f.is_ascii_lowercase() => f.to_ascii_uppercase().to_string() + c.as_str(),
        Some(f) => f.to_string() + c.as_str(),
        None => String::new(),
    }
}

/// Turns a reverse-DNS label into something readable.
/// "com.google.keystone.agent" -> "Google Keystone Agent", "homebrew.mxcl.mysql" -> "mysql (Homebrew)".
pub fn pretty_label(label: &str) -> String {
    if let Some(rest) = label.strip_prefix("homebrew.mxcl.") {
        return format!("{rest} (Homebrew)");
    }
    let mut parts: Vec<&str> = label.split('.').filter(|p| !p.is_empty()).collect();
    const TLDS: [&str; 12] = ["com", "org", "net", "io", "app", "dev", "local", "jp", "de", "uk", "us", "co"];
    while parts.len() > 1 && TLDS.contains(&parts[0].to_ascii_lowercase().as_str()) {
        parts.remove(0);
    }
    // Drop a repeated vendor ("ollama.ollama" -> "ollama").
    if parts.len() > 1 && parts[0].eq_ignore_ascii_case(parts[1]) {
        parts.remove(0);
    }
    let words: Vec<String> = parts.iter().flat_map(|p| p.split(['-', '_'])).filter(|w| !w.is_empty()).map(cap).collect();
    if words.is_empty() {
        label.to_string()
    } else {
        words.join(" ")
    }
}

pub fn is_own(label: &str) -> bool {
    label.starts_with("local.cleanyou.")
}

/// Labels we can safely pass to a shell (no quoting tricks).
pub fn safe_label(label: &str) -> bool {
    !label.is_empty() && label.chars().all(|c| c.is_ascii_alphanumeric() || "._-@+:".contains(c))
}

/// Reads one launchd plist. `None` for Apple's own jobs or unreadable files.
pub fn read_job(path: &Path, scope: Scope, disabled: &HashMap<String, bool>) -> Option<Job> {
    let v = plist::Value::from_file(path).ok()?;
    let d = v.as_dictionary()?;
    let label = d
        .get("Label")
        .and_then(|l| l.as_string())
        .map(str::to_string)
        .or_else(|| path.file_stem().map(|s| s.to_string_lossy().into_owned()))?;
    if label.starts_with("com.apple.") {
        return None;
    }
    let program = d
        .get("Program")
        .and_then(|p| p.as_string())
        .or_else(|| d.get("ProgramArguments").and_then(|a| a.as_array()).and_then(|a| a.first()).and_then(|p| p.as_string()))
        .unwrap_or("")
        .to_string();
    let run_at_load = d.get("RunAtLoad").and_then(|b| b.as_boolean()).unwrap_or(false);
    let keep_alive = match d.get("KeepAlive") {
        Some(plist::Value::Boolean(b)) => *b,
        Some(plist::Value::Dictionary(_)) => true,
        _ => false,
    };
    let plist_disabled = d.get("Disabled").and_then(|b| b.as_boolean()).unwrap_or(false);
    let enabled = !disabled.get(&label).copied().unwrap_or(plist_disabled);
    let app = if is_own(&label) { Some("Clean You".to_string()) } else { app_from_path(&program) };
    Some(Job { label, plist: path.to_path_buf(), program, app, scope, run_at_load, keep_alive, enabled })
}

pub fn read_dir_jobs(dir: &Path, scope: Scope, disabled: &HashMap<String, bool>) -> Vec<Job> {
    let Ok(rd) = std::fs::read_dir(dir) else { return vec![] };
    let mut jobs: Vec<Job> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "plist"))
        .filter_map(|p| read_job(&p, scope, disabled))
        .collect();
    jobs.sort_by_key(|j| j.label.to_lowercase());
    jobs
}

fn esc(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn run(cmd: &str, args: &[&str]) -> Result<String, String> {
    let o = Command::new(cmd).args(args).output().map_err(|e| e.to_string())?;
    if o.status.success() {
        Ok(String::from_utf8_lossy(&o.stdout).into_owned())
    } else {
        let e = String::from_utf8_lossy(&o.stderr).trim().to_string();
        Err(if e.is_empty() { format!("exit {}", o.status.code().unwrap_or(-1)) } else { e })
    }
}

fn osa(script: &str) -> Result<String, String> {
    run("/usr/bin/osascript", &["-e", script])
}

fn uid() -> u32 {
    run("/usr/bin/id", &["-u"]).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(501)
}

fn load() -> Snapshot {
    let uid = uid();
    let mut s = Snapshot { uid, ..Default::default() };
    match osa("tell application \"System Events\" to get the name of every login item") {
        Ok(out) => {
            let names = parse_as_list(&out);
            let paths = osa("tell application \"System Events\" to get the path of every login item").map(|p| parse_as_list(&p)).unwrap_or_default();
            let same = paths.len() == names.len();
            s.login = names
                .into_iter()
                .enumerate()
                .map(|(i, name)| LoginItem { name, path: if same { Some(paths[i].clone()).filter(|p| p.starts_with('/')) } else { None }, on: true })
                .collect();
        }
        Err(e) => {
            s.login_err = Some(if e.contains("-1743") || e.contains("Not authorized") {
                "Clean You isn't allowed to read login items. Allow it under Privacy & Security › Automation, or manage them in System Settings.".into()
            } else {
                "Couldn't read login items. You can manage them in System Settings.".into()
            });
        }
    }
    let user = parse_disabled(&run("/bin/launchctl", &["print-disabled", &format!("gui/{uid}")]).unwrap_or_default());
    let system = parse_disabled(&run("/bin/launchctl", &["print-disabled", "system"]).unwrap_or_default());
    s.jobs.extend(read_dir_jobs(&scan::home().join("Library/LaunchAgents"), Scope::User, &user));
    s.jobs.extend(read_dir_jobs(Path::new("/Library/LaunchAgents"), Scope::AllUsers, &user));
    s.jobs.extend(read_dir_jobs(Path::new("/Library/LaunchDaemons"), Scope::System, &system));
    s
}

/// Turns a launchd job on or off. Errors are human readable.
fn set_job(job: &Job, uid: u32, on: bool) -> Result<(), String> {
    if !safe_label(&job.label) || job.plist.to_string_lossy().contains(['\'', '"']) {
        return Err("unusual name, change it in System Settings".into());
    }
    let l = &job.label;
    let plist = job.plist.to_string_lossy();
    match job.scope {
        Scope::System => {
            let cmd = if on {
                format!("/bin/launchctl enable system/{l} && (/bin/launchctl bootstrap system '{plist}' || true)")
            } else {
                format!("(/bin/launchctl bootout system/{l} || true); /bin/launchctl disable system/{l}")
            };
            system::admin(&cmd)
        }
        Scope::User | Scope::AllUsers => {
            let target = format!("gui/{uid}/{l}");
            if on {
                run("/bin/launchctl", &["enable", &target])?;
                let _ = run("/bin/launchctl", &["bootstrap", &format!("gui/{uid}"), &plist]);
            } else {
                let _ = run("/bin/launchctl", &["bootout", &target]);
                run("/bin/launchctl", &["disable", &target])?;
            }
            Ok(())
        }
    }
}

fn set_login(item: &LoginItem, on: bool) -> Result<(), String> {
    if on {
        let p = item.path.as_deref().ok_or("unknown app location")?;
        osa(&format!("tell application \"System Events\" to make login item at end with properties {{path:\"{}\", hidden:false}}", esc(p)))?;
    } else {
        osa(&format!("tell application \"System Events\" to delete login item \"{}\"", esc(&item.name)))?;
    }
    Ok(())
}

// ---------- UI ----------

const GREEN: Color32 = Color32::from_rgb(48, 209, 88);

fn toggle(ui: &mut Ui, on: bool, enabled: bool) -> bool {
    let (rect, resp) = ui.allocate_exact_size(vec2(42.0, 24.0), if enabled { Sense::click() } else { Sense::hover() });
    let t = ui.ctx().animate_bool(resp.id, on);
    let p = ui.painter();
    let fill = if on { GREEN } else { TRACK };
    p.rect_filled(rect, 12.0, if enabled { fill } else { fill.gamma_multiply(0.45) });
    let x = rect.left() + 12.0 + t * (rect.width() - 24.0);
    p.circle_filled(pos2(x, rect.center().y), 9.5, if enabled { Color32::WHITE } else { Color32::from_gray(150) });
    enabled && resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
}

fn pill(ui: &mut Ui, text: &str, color: Color32) {
    let g = ui.painter().layout_no_wrap(text.to_string(), FontId::proportional(11.0), color);
    let (rect, _) = ui.allocate_exact_size(g.size() + vec2(16.0, 8.0), Sense::hover());
    ui.painter().rect_filled(rect, rect.height() / 2.0, color.gamma_multiply(0.16));
    ui.painter().galley(rect.center() - g.size() / 2.0, g, color);
}

/// What to draw for a startup item: its app's icon when we can find the app, else a fitting symbol.
fn item_icon(program: &str, label: &str, app: Option<&str>) -> (Option<PathBuf>, &'static str) {
    if is_own(label) {
        let me = std::env::current_exe().ok().and_then(|p| thumbs::bundle_of(&p.to_string_lossy()));
        return (me, ic::SPARKLE);
    }
    if let Some(b) = thumbs::bundle_of(program) {
        return (Some(b), ic::APP_WINDOW);
    }
    if let Some(name) = app {
        for dir in ["/Applications".into(), scan::home().join("Applications")] {
            let p: PathBuf = PathBuf::from(dir).join(format!("{name}.app"));
            if p.exists() {
                return (Some(p), ic::APP_WINDOW);
            }
        }
    }
    let l = format!("{label} {program}").to_lowercase();
    let glyph = if ["mysql", "postgres", "mongo", "redis", "mariadb", "elasticsearch"].iter().any(|d| l.contains(d)) {
        ic::DATABASE
    } else if l.contains("homebrew") || program.starts_with("/opt/") || program.starts_with("/usr/") {
        ic::TERMINAL_WINDOW
    } else if l.contains("update") || l.contains("keystone") {
        ic::ARROWS_CLOCKWISE
    } else {
        ic::GEAR_SIX
    };
    (None, glyph)
}

fn scope_info(s: Scope) -> (&'static str, &'static str, Color32) {
    match s {
        Scope::User => (ic::USER, "You", ACCENT),
        Scope::AllUsers => (ic::USERS, "All users", Color32::from_rgb(191, 90, 242)),
        Scope::System => (ic::GEAR, "System", WARN),
    }
}

enum Act {
    Job(Job, bool),
    Login(LoginItem, bool),
    Reveal(PathBuf),
}

impl App {
    fn startup_refresh(&mut self, ctx: &egui::Context) {
        if self.startup.loading {
            return;
        }
        self.startup.loading = true;
        let slot = self.startup.incoming.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let s = load();
            *slot.lock().unwrap() = Some(s);
            ctx.request_repaint();
        });
    }

    fn startup_apply(&mut self, ctx: &egui::Context, act: Act) {
        let (key, work): (String, Box<dyn FnOnce() -> String + Send>) = match act {
            Act::Reveal(p) => {
                system::reveal(&p);
                return;
            }
            Act::Job(job, on) => {
                let uid = self.startup.snap.uid;
                let name = job.app.clone().unwrap_or_else(|| pretty_label(&job.label));
                (job.label.clone(), Box::new(move || match set_job(&job, uid, on) {
                    Ok(()) if on => format!("{name} will start automatically"),
                    Ok(()) => format!("{name} won't start automatically"),
                    Err(e) if e.contains("-128") => "Cancelled".into(),
                    Err(e) => format!("Couldn't change {name}: {e}"),
                }))
            }
            Act::Login(item, on) => {
                if !on {
                    self.startup.removed_login.retain(|r| r.name != item.name);
                    self.startup.removed_login.push(LoginItem { on: false, ..item.clone() });
                } else {
                    self.startup.removed_login.retain(|r| r.name != item.name);
                }
                let name = item.name.clone();
                (format!("login:{name}"), Box::new(move || match set_login(&item, on) {
                    Ok(()) if on => format!("{name} will open at login"),
                    Ok(()) => format!("{name} removed from login items"),
                    Err(e) => format!("Couldn't change {name}: {e}"),
                }))
            }
        };
        self.startup.busy.insert(key);
        self.startup.loading = true;
        let (tx, slot, ctx) = (self.tx.clone(), self.startup.incoming.clone(), ctx.clone());
        std::thread::spawn(move || {
            let msg = work();
            let _ = tx.send(Msg::Status(msg));
            *slot.lock().unwrap() = Some(load());
            ctx.request_repaint();
        });
    }

    pub(crate) fn startup_page(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        let f = ctx.cumulative_pass_nr();
        if self.startup.last_frame.map_or(true, |l| f > l + 1) {
            self.startup_refresh(&ctx);
        }
        self.startup.last_frame = Some(f);
        if let Some(s) = self.startup.incoming.lock().unwrap().take() {
            self.startup.snap = s;
            self.startup.loading = false;
            self.startup.busy.clear();
        }

        let loading = self.startup.loading;
        let mut refresh = false;
        header(ui, ic::POWER, WARN, "Startup Items", "Apps and background helpers that launch on their own when you log in.", |ui| {
            if loading {
                ui.add(egui::Spinner::new().size(16.0).color(DIM));
            } else if icon_button(ui, ic::ARROWS_CLOCKWISE, DIM, "Refresh").clicked() {
                refresh = true;
            }
        });
        if refresh {
            self.startup_refresh(&ctx);
        }

        // Merge login items removed this session so they can be switched back on.
        let mut login = self.startup.snap.login.clone();
        for r in &self.startup.removed_login {
            if !login.iter().any(|l| l.name == r.name) {
                login.push(r.clone());
            }
        }
        let snap = &self.startup.snap;
        let busy = &self.startup.busy;
        let thumbs = &mut self.thumbs;
        let mut act: Option<Act> = None;

        let on_jobs = snap.jobs.iter().filter(|j| j.enabled).count();
        let on_login = login.iter().filter(|l| l.on).count();
        let total = on_jobs + on_login;

        egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
            card(ui, |ui| {
                ui.horizontal(|ui| {
                    badge(ui, ic::ROCKET_LAUNCH, WARN, 40.0);
                    ui.vertical(|ui| {
                        let t = if snap.jobs.is_empty() && login.is_empty() && loading {
                            "Looking for startup items…".to_string()
                        } else {
                            format!("{total} item{} start automatically", if total == 1 { "" } else { "s" })
                        };
                        ui.label(RichText::new(t).size(18.0).strong().color(Color32::WHITE));
                        let agents = snap.jobs.iter().filter(|j| j.enabled && j.scope != Scope::System).count();
                        let daemons = snap.jobs.iter().filter(|j| j.enabled && j.scope == Scope::System).count();
                        ui.label(RichText::new(format!("{on_login} login items · {agents} background agents · {daemons} system daemons")).color(DIM));
                    });
                });
                ui.add_space(4.0);
                ui.label(RichText::new("Turning something off doesn't uninstall it. It just stops it launching by itself. You can turn it back on anytime.").small().color(DIM));
            });

            // ----- login items -----
            ui.add_space(14.0);
            ui.label(RichText::new("LOGIN ITEMS").size(11.0).strong().color(DIM));
            ui.add_space(4.0);
            card(ui, |ui| {
                if let Some(err) = &snap.login_err {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new(format!("{} {err}", ic::INFO)).color(WARN));
                    });
                    if ui.button(format!("{}  Open Login Items settings", ic::GEAR)).clicked() {
                        system::open(LOGIN_SETTINGS);
                    }
                } else if login.is_empty() {
                    ui.label(RichText::new(if loading { "Loading…" } else { "No login items." }).color(DIM));
                } else {
                    for (i, it) in login.iter().enumerate() {
                        if i > 0 {
                            ui.separator();
                        }
                        let key = format!("login:{}", it.name);
                        let sub = it.path.as_deref().map(|p| short(Path::new(p))).unwrap_or_else(|| "Opens when you log in".into());
                        let can = !busy.contains(&key) && (it.on || it.path.is_some());
                        let icon = (it.path.as_deref().map(PathBuf::from), ic::APP_WINDOW);
                        row(ui, thumbs, icon, ACCENT, &it.name, &sub, None, it.on, can, busy.contains(&key), it.path.as_ref().map(PathBuf::from), |a| {
                            act = Some(match a {
                                RowAct::Toggle => Act::Login(it.clone(), !it.on),
                                RowAct::Reveal(p) => Act::Reveal(p),
                            })
                        });
                    }
                    ui.add_space(4.0);
                    if ui.link(RichText::new("More in System Settings › Login Items").small().color(DIM)).clicked() {
                        system::open(LOGIN_SETTINGS);
                    }
                }
            });

            // ----- launchd jobs -----
            for (title, scopes) in [
                ("BACKGROUND AGENTS", &[Scope::User, Scope::AllUsers][..]),
                ("SYSTEM DAEMONS", &[Scope::System][..]),
            ] {
                let jobs: Vec<&Job> = snap.jobs.iter().filter(|j| scopes.contains(&j.scope)).collect();
                if jobs.is_empty() {
                    continue;
                }
                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new(title).size(11.0).strong().color(DIM));
                    if scopes.contains(&Scope::System) {
                        ui.label(RichText::new(format!("{} needs your password", ic::LOCK)).size(11.0).color(DIM));
                    }
                });
                ui.add_space(4.0);
                card(ui, |ui| {
                    for (i, j) in jobs.iter().enumerate() {
                        if i > 0 {
                            ui.separator();
                        }
                        let (_, scope_txt, color) = scope_info(j.scope);
                        let icon = item_icon(&j.program, &j.label, j.app.as_deref());
                        let name = j.app.clone().unwrap_or_else(|| pretty_label(&j.label));
                        let mut sub = if j.program.is_empty() { j.label.clone() } else { short(Path::new(&j.program)) };
                        if j.app.is_some() {
                            // Several helpers can belong to one app, so name the helper too.
                            sub = format!("{} · {sub}", pretty_label(&j.label));
                        }
                        let mut tags = vec![];
                        if j.run_at_load {
                            tags.push("at login");
                        }
                        if j.keep_alive {
                            tags.push("always running");
                        }
                        if !tags.is_empty() {
                            sub = format!("{} · {sub}", tags.join(", "));
                        }
                        let own = is_own(&j.label);
                        if own {
                            sub = "Clean You's own helper. Turn it off in Battery Care or Smart Alerts.".into();
                        }
                        let is_busy = busy.contains(&j.label);
                        let lock = (j.scope == Scope::System).then_some(scope_txt);
                        let pill_txt = if lock.is_some() { format!("{} {scope_txt}", ic::LOCK) } else { scope_txt.to_string() };
                        row(ui, thumbs, icon, color, &name, &sub, Some((&pill_txt, color)), j.enabled, !own && !is_busy, is_busy, Some(j.plist.clone()), |a| {
                            act = Some(match a {
                                RowAct::Toggle => Act::Job((*j).clone(), !j.enabled),
                                RowAct::Reveal(p) => Act::Reveal(p),
                            })
                        });
                    }
                });
            }
        });

        if let Some(a) = act {
            self.startup_apply(&ctx, a);
        }
    }
}

enum RowAct {
    Toggle,
    Reveal(PathBuf),
}

#[allow(clippy::too_many_arguments)]
fn row(
    ui: &mut Ui,
    thumbs: &mut thumbs::Thumbs,
    icon: (Option<PathBuf>, &str),
    color: Color32,
    title: &str,
    sub: &str,
    tag: Option<(&str, Color32)>,
    on: bool,
    can_toggle: bool,
    busy: bool,
    reveal: Option<PathBuf>,
    mut out: impl FnMut(RowAct),
) {
    ui.allocate_ui_with_layout(vec2(ui.available_width(), 50.0), Layout::right_to_left(Align::Center), |ui| {
        if busy {
            ui.add_sized(vec2(42.0, 24.0), egui::Spinner::new().size(16.0).color(DIM));
        } else if toggle(ui, on, can_toggle) {
            out(RowAct::Toggle);
        }
        ui.add_space(6.0);
        if let Some(p) = reveal {
            if icon_button(ui, ic::FOLDER_OPEN, DIM, "Show in Finder").clicked() {
                out(RowAct::Reveal(p));
            }
        }
        if let Some((t, c)) = tag {
            pill(ui, t, c);
        }
        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
            let path = icon.0.unwrap_or_default();
            let r = thumbs::preview(ui, thumbs, &path, 34.0, icon.1, if on { color } else { DIM });
            if !on {
                // Fade switched-off items.
                ui.painter().rect_filled(r.rect, 8.0, Color32::from_black_alpha(110));
            }
            ui.vertical(|ui| {
                ui.add(Label::new(RichText::new(title).strong().color(if on { Color32::WHITE } else { DIM })).truncate());
                ui.add(Label::new(RichText::new(sub).small().color(DIM)).truncate());
            });
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_print_disabled() {
        let text = "\tdisabled services = {\n\t\t\"com.docker.helper\" => enabled\n\t\t\"com.foo.bar\" => disabled\n\t\t\"old.style\" => true\n\t\t\"old.on\" => false\n\t}\n\tlogin item associations = {\n\t\t\"com.x\" => something\n\t}\n";
        let m = parse_disabled(text);
        assert_eq!(m.get("com.docker.helper"), Some(&false));
        assert_eq!(m.get("com.foo.bar"), Some(&true));
        assert_eq!(m.get("old.style"), Some(&true));
        assert_eq!(m.get("old.on"), Some(&false));
        assert!(!m.contains_key("com.x"));
        assert_eq!(m.len(), 4);
        assert!(parse_disabled("").is_empty());
    }

    #[test]
    fn parses_applescript_lists() {
        assert_eq!(parse_as_list("Raycast, Docker\n"), vec!["Raycast", "Docker"]);
        assert!(parse_as_list("\n").is_empty());
        assert_eq!(parse_as_list("One"), vec!["One"]);
    }

    #[test]
    fn names() {
        assert_eq!(app_from_path("/Applications/Docker.app/Contents/MacOS/com.docker.vmnetd"), Some("Docker".into()));
        assert_eq!(app_from_path("/usr/local/bin/foo"), None);
        assert_eq!(pretty_label("com.google.keystone.agent"), "Google Keystone Agent");
        assert_eq!(pretty_label("com.ollama.ollama"), "Ollama");
        assert_eq!(pretty_label("homebrew.mxcl.postgresql@14"), "postgresql@14 (Homebrew)");
        assert_eq!(pretty_label("jp.co.canon.CUPSSFP.BG"), "Canon CUPSSFP BG");
        assert_eq!(pretty_label("single"), "Single");
        assert!(is_own("local.cleanyou.agent") && is_own("local.cleanyou.charge") && !is_own("local.other"));
        assert!(safe_label("homebrew.mxcl.postgresql@14"));
        assert!(!safe_label("a b") && !safe_label("x'; rm") && !safe_label(""));
    }

    #[test]
    fn reads_plists() {
        let dir = std::env::temp_dir().join(format!("cleanyou-startup-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut d = plist::Dictionary::new();
        d.insert("Label".into(), "com.acme.helper".into());
        d.insert("ProgramArguments".into(), plist::Value::Array(vec!["/Applications/Acme.app/Contents/MacOS/helper".into(), "--x".into()]));
        d.insert("RunAtLoad".into(), true.into());
        d.insert("KeepAlive".into(), plist::Value::Dictionary(plist::Dictionary::new()));
        plist::Value::Dictionary(d).to_file_xml(dir.join("com.acme.helper.plist")).unwrap();
        let mut a = plist::Dictionary::new();
        a.insert("Label".into(), "com.apple.something".into());
        plist::Value::Dictionary(a).to_file_xml(dir.join("com.apple.something.plist")).unwrap();
        plist::Value::Dictionary(plist::Dictionary::new()).to_file_xml(dir.join("com.empty.job.plist")).unwrap();
        std::fs::write(dir.join("notes.txt"), "x").unwrap();

        let mut dis = HashMap::new();
        dis.insert("com.empty.job".to_string(), true);
        let jobs = read_dir_jobs(&dir, Scope::User, &dis);
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(jobs.len(), 2);
        let h = jobs.iter().find(|j| j.label == "com.acme.helper").unwrap();
        assert_eq!(h.app.as_deref(), Some("Acme"));
        assert!(h.run_at_load && h.keep_alive && h.enabled);
        let e = jobs.iter().find(|j| j.label == "com.empty.job").unwrap();
        assert!(!e.enabled && e.program.is_empty());
    }

    /// Read-only look at this Mac: `cargo test -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn smoke_load() {
        let s = load();
        println!("uid={} login={:?} err={:?}", s.uid, s.login, s.login_err);
        for j in s.jobs {
            println!("{:?} {:<45} on={} app={:?} rl={} ka={} {}", j.scope, j.label, j.enabled, j.app, j.run_at_load, j.keep_alive, pretty_label(&j.label));
        }
    }
}
