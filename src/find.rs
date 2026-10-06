//! Find: every file and folder on the drive, hidden ones too, searchable as you type.

use super::*;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Condvar;

const NO_PARENT: u32 = u32::MAX;
const SHOWN: usize = 300;
/// Never descended into when indexing the whole Mac: other volumes, the data volume seen
/// a second time through /System/Volumes, and device/automount folders.
const SKIP_WHOLE: [&str; 7] = ["/System/Volumes", "/Volumes", "/dev", "/net", "/home", "/Network", "/private/var/vm"];

/// One file or folder. Paths are rebuilt from parents, so millions of entries stay small.
pub struct Entry {
    parent: u32,
    /// File name, or the full path for a root.
    name: Box<str>,
    /// Bytes on disk; folders get their total once indexing finishes.
    size: u64,
    dir: bool,
}

#[derive(Clone, Copy, PartialEq)]
pub enum Scope {
    WholeMac,
    Home,
    Folder,
}

#[derive(Clone, Copy, PartialEq, Default)]
enum Kind {
    #[default]
    All,
    Files,
    Folders,
}

#[derive(Default)]
struct Shared {
    entries: Mutex<Vec<Entry>>,
    count: AtomicU64,
    done: AtomicBool,
    stop: AtomicBool,
}

fn path_of(entries: &[Entry], mut i: u32) -> PathBuf {
    let mut parts = Vec::new();
    while i != NO_PARENT {
        let e = &entries[i as usize];
        parts.push(&*e.name);
        i = e.parent;
    }
    let mut p = PathBuf::new();
    for part in parts.into_iter().rev() {
        p.push(part);
    }
    p
}

/// Walks `roots` with a pool of threads; each directory is listed by whichever thread is free.
fn build(roots: Vec<PathBuf>, skip: Vec<PathBuf>, shared: &Shared) {
    let queue: Mutex<(Vec<(u32, PathBuf)>, usize)> = Mutex::new((Vec::new(), 0));
    {
        let mut entries = shared.entries.lock().unwrap();
        let mut q = queue.lock().unwrap();
        for r in roots {
            q.0.push((entries.len() as u32, r.clone()));
            entries.push(Entry { parent: NO_PARENT, name: r.to_string_lossy().into(), size: 0, dir: true });
        }
    }
    let cv = Condvar::new();
    let workers = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(2, 12);
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| loop {
                let job = {
                    let mut q = queue.lock().unwrap();
                    loop {
                        if shared.stop.load(Ordering::Relaxed) {
                            break None;
                        }
                        if let Some(j) = q.0.pop() {
                            q.1 += 1;
                            break Some(j);
                        }
                        if q.1 == 0 {
                            break None;
                        }
                        q = cv.wait(q).unwrap();
                    }
                };
                let Some((idx, dir)) = job else {
                    cv.notify_all();
                    break;
                };
                let mut batch = Vec::new();
                let mut subdirs = Vec::new();
                if let Ok(rd) = fs::read_dir(&dir) {
                    for e in rd.flatten() {
                        // DirEntry::metadata does not follow symlinks.
                        let Ok(m) = e.metadata() else { continue };
                        let is_dir = m.is_dir();
                        if is_dir {
                            let p = e.path();
                            if !skip.iter().any(|s| *s == p) {
                                subdirs.push((batch.len(), p));
                            }
                        }
                        let size = if is_dir { 0 } else { m.blocks() * 512 };
                        batch.push(Entry { parent: idx, name: e.file_name().to_string_lossy().into(), size, dir: is_dir });
                    }
                }
                shared.count.fetch_add(batch.len() as u64, Ordering::Relaxed);
                let base = {
                    let mut entries = shared.entries.lock().unwrap();
                    let b = entries.len();
                    entries.extend(batch);
                    b
                };
                let mut q = queue.lock().unwrap();
                q.0.extend(subdirs.into_iter().map(|(k, p)| ((base + k) as u32, p)));
                q.1 -= 1;
                cv.notify_all();
            });
        }
    });
    if shared.stop.load(Ordering::Relaxed) {
        return;
    }
    // Children always come after their parent, so one backwards pass totals every folder.
    let mut entries = shared.entries.lock().unwrap();
    for i in (0..entries.len()).rev() {
        let (parent, size) = (entries[i].parent, entries[i].size);
        if parent != NO_PARENT {
            entries[parent as usize].size += size;
        }
    }
    drop(entries);
    shared.done.store(true, Ordering::Relaxed);
}

/// Case-insensitive `contains`; `needle` is already lowercase.
fn contains_ci(hay: &str, needle: &str) -> bool {
    if hay.is_ascii() && needle.is_ascii() {
        let (h, n) = (hay.as_bytes(), needle.as_bytes());
        n.len() <= h.len() && h.windows(n.len()).any(|w| w.iter().zip(n).all(|(a, b)| a.to_ascii_lowercase() == *b))
    } else {
        hay.to_lowercase().contains(needle)
    }
}

/// Matching entries (every word must be in the name), largest first. Returns (shown, total matches).
fn search(entries: &[Entry], query: &str, kind: Kind) -> (Vec<u32>, usize) {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    if words.is_empty() {
        return (Vec::new(), 0);
    }
    let mut hits: Vec<u32> = entries
        .iter()
        .enumerate()
        .filter(|(_, e)| e.parent != NO_PARENT)
        .filter(|(_, e)| match kind {
            Kind::All => true,
            Kind::Files => !e.dir,
            Kind::Folders => e.dir,
        })
        .filter(|(_, e)| words.iter().all(|w| contains_ci(&e.name, w)))
        .map(|(i, _)| i as u32)
        .collect();
    let total = hits.len();
    hits.sort_by(|a, b| entries[*b as usize].size.cmp(&entries[*a as usize].size));
    hits.truncate(SHOWN);
    (hits, total)
}

fn thousands(n: u64) -> String {
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

pub struct State {
    shared: Arc<Shared>,
    scope: Scope,
    folder: Option<PathBuf>,
    started: Option<Instant>,
    took: Option<Duration>,
    query: String,
    kind: Kind,
    /// (query, kind, entry count) the results were computed for.
    searched: (String, Kind, usize),
    last_search: Option<Instant>,
    results: Vec<(PathBuf, u64, bool)>,
    total: usize,
    pub inspect: inspect::State,
}

impl Default for State {
    fn default() -> Self {
        State {
            shared: Arc::default(),
            scope: Scope::WholeMac,
            folder: None,
            started: None,
            took: None,
            query: String::new(),
            kind: Kind::All,
            searched: (String::new(), Kind::All, 0),
            last_search: None,
            results: Vec::new(),
            total: 0,
            inspect: Default::default(),
        }
    }
}

impl State {
    fn indexing(&self) -> bool {
        self.started.is_some() && !self.shared.done.load(Ordering::Relaxed)
    }

    fn start(&mut self, ctx: &egui::Context) {
        self.shared.stop.store(true, Ordering::Relaxed);
        let (roots, skip): (Vec<PathBuf>, Vec<PathBuf>) = match (self.scope, &self.folder) {
            (Scope::Home, _) => (vec![scan::home()], vec![]),
            (Scope::Folder, Some(f)) => (vec![f.clone()], vec![]),
            _ => (vec![PathBuf::from("/")], SKIP_WHOLE.iter().map(PathBuf::from).collect()),
        };
        let shared = Arc::new(Shared::default());
        self.shared = shared.clone();
        self.started = Some(Instant::now());
        self.took = None;
        self.searched = (String::new(), Kind::All, 0);
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            build(roots, skip, &shared);
            ctx.request_repaint();
        });
    }

    /// Re-runs the search when the query changes, and every second while the index grows.
    fn refresh(&mut self) {
        let done = self.shared.done.load(Ordering::Relaxed);
        if done && self.took.is_none() {
            self.took = self.started.map(|s| s.elapsed());
        }
        let count = self.shared.count.load(Ordering::Relaxed) as usize;
        let key = (self.query.clone(), self.kind, if done { usize::MAX } else { count });
        let stale_ok = self.last_search.is_some_and(|t| t.elapsed() < Duration::from_secs(1)) && !done;
        if key == self.searched || (key.0 == self.searched.0 && key.1 == self.searched.1 && stale_ok) {
            return;
        }
        let entries = self.shared.entries.lock().unwrap();
        let (hits, total) = search(&entries, &self.query, self.kind);
        self.results = hits.iter().map(|&i| (path_of(&entries, i), entries[i as usize].size, entries[i as usize].dir)).collect();
        self.total = total;
        drop(entries);
        self.searched = key;
        self.last_search = Some(Instant::now());
    }

    /// (name, size) of `path`'s parent folder from the index, once sizes are known.
    fn parent_info(&self, path: &Path) -> Option<(String, u64)> {
        let parent = path.parent()?;
        let size = self.results.iter().find(|r| r.0 == parent).map(|r| r.1).or_else(|| {
            if !self.shared.done.load(Ordering::Relaxed) {
                return None;
            }
            let entries = self.shared.entries.lock().unwrap();
            let name = parent.file_name()?.to_string_lossy();
            entries
                .iter()
                .enumerate()
                .filter(|(_, e)| e.dir && *e.name == *name)
                .find(|(i, _)| path_of(&entries, *i as u32) == parent)
                .map(|(_, e)| e.size)
        })?;
        Some((parent.file_name()?.to_string_lossy().into_owned(), size))
    }
}

enum Act {
    Rescan,
    Scope(Scope),
    Inspect(PathBuf),
    Stage(PathBuf, String, u64),
    Reveal(PathBuf),
    Look(PathBuf),
    Open(PathBuf),
}

impl App {
    pub(crate) fn find_page(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        if self.find.started.is_none() {
            self.find.start(&ctx);
        }
        self.find.refresh();
        let indexing = self.find.indexing();
        if indexing {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
        let mut act = None;
        let teal = Color32::from_rgb(102, 212, 207);
        header(ui, ic::MAGNIFYING_GLASS, teal, "Find", "Any file or folder, hidden ones too. Results appear as you type, biggest first.", |ui| {
            let b = Button::new(format!("{}  Re-index", ic::ARROWS_CLOCKWISE)).rounding(9.0).min_size(vec2(0.0, 34.0));
            if ui.add_enabled(!indexing, b).clicked() {
                act = Some(Act::Rescan);
            }
        });
        ui.horizontal(|ui| {
            ui.label(RichText::new("Search in").color(DIM));
            for (s, t) in [(Scope::WholeMac, format!("{}  Whole Mac", ic::HARD_DRIVE)), (Scope::Home, format!("{}  Home", ic::HOUSE))] {
                if chip(ui, self.find.scope == s, &t) {
                    act = Some(Act::Scope(s));
                }
            }
            let folder = match (&self.find.folder, self.find.scope) {
                (Some(f), Scope::Folder) => format!("{}  {}", ic::FOLDER_OPEN, f.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()),
                _ => format!("{}  Choose folder…", ic::FOLDER_OPEN),
            };
            if chip(ui, self.find.scope == Scope::Folder, &folder) {
                act = Some(Act::Scope(Scope::Folder));
            }
        });
        ui.add_space(8.0);

        let st = &mut self.find;
        ui.horizontal(|ui| {
            let r = search_field(ui, &mut st.query, "Search every file and folder", 420.0);
            if st.query.is_empty() && !r.has_focus() && ui.memory(|m| m.focused().is_none()) {
                r.request_focus();
            }
            for (k, t) in [(Kind::All, "All"), (Kind::Files, "Files"), (Kind::Folders, "Folders")] {
                if chip(ui, st.kind == k, t) {
                    st.kind = k;
                }
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let n = thousands(st.shared.count.load(Ordering::Relaxed));
                let secs = st.started.map_or(0, |s| s.elapsed().as_secs());
                if indexing {
                    ui.label(RichText::new(format!("Indexing… {n} items · {secs}s")).color(DIM));
                    ui.add(egui::Spinner::new().size(14.0).color(teal));
                } else {
                    let took = st.took.map_or(0.0, |d| d.as_secs_f32());
                    ui.label(RichText::new(format!("{} {n} items indexed in {took:.0}s", ic::CHECK_CIRCLE)).color(DIM));
                }
            });
        });
        ui.add_space(8.0);
        if !st.query.trim().is_empty() {
            let more = if st.total > st.results.len() { format!(" · showing the {} largest", st.results.len()) } else { String::new() };
            ui.label(RichText::new(format!("{} matches{more}", thousands(st.total as u64))).small().color(DIM));
            ui.add_space(4.0);
        }

        let show_inspector = st.inspect.target.is_some();
        let w = ui.available_width();
        let list_w = if show_inspector && w >= 880.0 { w * 0.6 } else { w };
        let removed = &self.removed;
        let thumbs = &mut self.thumbs;
        let mut list = |ui: &mut Ui, act: &mut Option<Act>| {
            Frame::none().fill(CARD).rounding(14.0).stroke(Stroke::new(1.0_f32, BORDER)).inner_margin(8.0).show(ui, |ui| {
                ui.set_width(ui.available_width());
                egui::ScrollArea::vertical().id_salt("find-results").auto_shrink([false; 2]).show(ui, |ui| {
                    if st.query.trim().is_empty() {
                        ui.add_space(40.0);
                        ui.vertical_centered(|ui| {
                            ui.label(RichText::new(ic::MAGNIFYING_GLASS).size(40.0).color(DIM));
                            ui.label(RichText::new("Type part of a name").strong().color(Color32::WHITE));
                            ui.label(RichText::new("Try “.dmg”, “node_modules”, “backup” or “.mov”").color(DIM));
                        });
                        ui.add_space(40.0);
                        return;
                    }
                    if st.results.is_empty() {
                        ui.add_space(30.0);
                        ui.vertical_centered(|ui| {
                            ui.label(RichText::new(if indexing { "Nothing yet, still indexing…" } else { "No matches" }).color(DIM));
                        });
                        return;
                    }
                    for (path, size, dir) in &st.results {
                        if removed.contains(path) {
                            continue;
                        }
                        let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 48.0), Sense::click());
                        if !ui.is_rect_visible(rect) {
                            continue;
                        }
                        let selected = st.inspect.target.as_ref() == Some(path);
                        if selected {
                            ui.painter().rect_filled(rect, 9.0, ACCENT.gamma_multiply(0.16));
                        } else if resp.hovered() {
                            ui.painter().rect_filled(rect, 9.0, Color32::from_white_alpha(7));
                        }
                        let cy = rect.center().y;
                        let mut tui = ui.new_child(egui::UiBuilder::new().max_rect(Rect::from_min_size(pos2(rect.left() + 6.0, cy - 16.0), vec2(32.0, 32.0))));
                        thumbs::preview(&mut tui, thumbs, path, 32.0, if *dir { ic::FOLDER } else { ic::FILE }, teal);
                        let mut right = rect.right() - 4.0;
                        let mut btns = vec![(ic::FOLDER_OPEN, "Show in Finder", 0u8), (ic::INFO, "Inspect", 1)];
                        if !scan::protected(path) {
                            btns.push((ic::LIST_PLUS, "Add to Cleanup list", 2));
                        }
                        if !*dir && thumbs::previewable(path) {
                            btns.push((ic::EYE, "Quick Look", 3));
                        }
                        for (icon, tip, k) in btns {
                            let r = Rect::from_center_size(pos2(right - 15.0, cy), vec2(30.0, 30.0));
                            let b = ui.interact(r, Id::new(("find-btn", path, k)), Sense::click());
                            paint_icon_btn(ui, r, &b, icon, false);
                            if b.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(tip).clicked() {
                                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                                *act = Some(match k {
                                    0 => Act::Reveal(path.clone()),
                                    1 => Act::Inspect(path.clone()),
                                    2 => Act::Stage(path.clone(), name, *size),
                                    _ => Act::Look(path.clone()),
                                });
                            }
                            right -= 32.0;
                        }
                        right -= 6.0;
                        let size_text = if *dir && indexing { "…".to_string() } else { human(*size) };
                        ui.painter().text(pos2(right, cy), Align2::RIGHT_CENTER, size_text, FontId::proportional(13.5), Color32::WHITE);
                        right -= 84.0;
                        let x = rect.left() + 48.0;
                        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                        let g = truncated(ui, &disp(&name), 13.5, Color32::WHITE, right - x);
                        ui.painter().galley(pos2(x, cy - g.size().y - 1.0), g, Color32::WHITE);
                        let parent = path.parent().map(short).unwrap_or_default();
                        let g = truncated(ui, &parent, 11.0, DIM, right - x);
                        ui.painter().galley(pos2(x, cy + 2.0), g, DIM);
                        if resp.double_clicked() {
                            *act = Some(Act::Open(path.clone()));
                        } else if resp.on_hover_text(short(path)).clicked() {
                            *act = Some(Act::Inspect(path.clone()));
                        }
                    }
                });
            });
        };
        let mut iact = None;
        if show_inspector && w >= 880.0 {
            ui.horizontal_top(|ui| {
                ui.vertical(|ui| {
                    ui.set_width(list_w - 12.0);
                    list(ui, &mut act);
                });
                ui.add_space(4.0);
                ui.vertical(|ui| {
                    egui::ScrollArea::vertical().id_salt("find-inspect").auto_shrink([false; 2]).show(ui, |ui| {
                        iact = st.inspect.show(ui);
                    });
                });
            });
        } else {
            if show_inspector {
                iact = st.inspect.show(ui);
                ui.add_space(8.0);
            }
            list(ui, &mut act);
        }

        let act = act.or(match iact {
            Some(inspect::Act::Inspect(p)) => Some(Act::Inspect(p)),
            Some(inspect::Act::Stage(p, n, s)) => Some(Act::Stage(p, n, s)),
            Some(inspect::Act::Reveal(p)) => Some(Act::Reveal(p)),
            Some(inspect::Act::Close) => {
                self.find.inspect.close();
                None
            }
            None => None,
        });
        match act {
            Some(Act::Rescan) => self.find.start(&ctx),
            Some(Act::Scope(s)) => {
                if s == Scope::Folder {
                    match rfd::FileDialog::new().set_directory(scan::home()).pick_folder() {
                        Some(f) => self.find.folder = Some(f),
                        None => return,
                    }
                }
                self.find.scope = s;
                self.find.start(&ctx);
            }
            Some(Act::Inspect(p)) => {
                let parent = self.find.parent_info(&p);
                self.find.inspect.open(p, parent, &ctx);
            }
            Some(Act::Stage(p, n, s)) => self.stage(vec![(p, n, s)]),
            Some(Act::Reveal(p)) => system::reveal(&p),
            Some(Act::Look(p)) => system::quick_look(&p),
            Some(Act::Open(p)) => system::open_path(&p),
            None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indexes_and_searches() {
        let d = std::env::temp_dir().join(format!("cleanyou-find-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(d.join(".hidden/Deep")).unwrap();
        fs::write(d.join(".hidden/Deep/Report-Final.PDF"), vec![1u8; 300_000]).unwrap();
        fs::write(d.join("report-draft.txt"), b"x").unwrap();
        fs::create_dir_all(d.join("skipme")).unwrap();
        fs::write(d.join("skipme/report-inside-skipped"), b"x").unwrap();
        let shared = Shared::default();
        build(vec![d.clone()], vec![d.join("skipme")], &shared);
        assert!(shared.done.load(Ordering::Relaxed));
        let entries = shared.entries.into_inner().unwrap();

        let (hits, total) = search(&entries, "REPORT", Kind::All);
        assert_eq!(total, 2, "hidden folders are searched, skipped folders are not");
        assert_eq!(path_of(&entries, hits[0]), d.join(".hidden/Deep/Report-Final.PDF"), "largest first");
        assert_eq!(search(&entries, "report final", Kind::All).1, 1, "every word must match");
        assert_eq!(search(&entries, "deep", Kind::Files).1, 0);
        assert_eq!(search(&entries, "deep", Kind::Folders).1, 1);
        // Folder sizes include everything inside.
        let (deep, _) = search(&entries, "deep", Kind::Folders);
        assert!(entries[deep[0] as usize].size >= 300_000);
        assert!(entries[0].size >= 300_000);
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn helpers() {
        assert!(contains_ci("Hello.DMG", ".dmg"));
        assert!(!contains_ci("a", "ab"));
        assert!(contains_ci("Ünïcode Fïle", "fïle"));
        assert_eq!(thousands(1234567), "1,234,567");
        assert_eq!(thousands(12), "12");
    }
}
