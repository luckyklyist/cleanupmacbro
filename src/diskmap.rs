//! Disk Map: sunburst view of the drive, plus snapshots of what grew.

use super::*;
use egui::{Mesh, Pos2, UiBuilder};
use std::collections::HashMap;
use std::f32::consts::{FRAC_PI_2, TAU};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// Children smaller than this are merged into "smaller items" on the first scan.
const FIRST_THRESH: u64 = 20 * MB;
/// Rings drawn around the focused folder.
const RINGS: usize = 3;
const KEEP_SNAPSHOTS: usize = 10;
/// Ignore changes smaller than this in "What grew".
const MIN_CHANGE: u64 = 10 * MB;
const PACKAGES: [&str; 15] = [
    "app", "photoslibrary", "photolibrary", "musiclibrary", "tvlibrary", "imovielibrary", "fcpbundle", "logicx", "band",
    "utm", "vmwarevm", "pvm", "sparsebundle", "xcarchive", "bundle",
];
const PALETTE: [Color32; 10] = [
    Color32::from_rgb(10, 132, 255),
    Color32::from_rgb(191, 90, 242),
    Color32::from_rgb(255, 55, 95),
    Color32::from_rgb(255, 159, 10),
    Color32::from_rgb(48, 209, 88),
    Color32::from_rgb(100, 210, 255),
    Color32::from_rgb(255, 214, 10),
    Color32::from_rgb(94, 92, 230),
    Color32::from_rgb(172, 142, 104),
    Color32::from_rgb(102, 212, 207),
];
const GRAY_OTHER: Color32 = Color32::from_rgb(98, 98, 106);
const GRAY_SMALL: Color32 = Color32::from_rgb(70, 70, 77);

// ---------- size tree ----------

#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    Dir,
    File,
    Package,
    /// Merged leaf holding this many items below the threshold.
    Smaller(u32),
    /// Synthetic leaf (e.g. "System & other").
    Other,
}

#[derive(Debug)]
pub struct Node {
    name: String,
    path: Option<PathBuf>,
    size: u64,
    kind: Kind,
    /// Children below this size were merged into one "smaller items" leaf.
    thresh: u64,
    /// Sorted by size (largest first), with the "smaller items" leaf last.
    children: Vec<Node>,
}

impl Node {
    fn leaf(name: impl Into<String>, path: Option<PathBuf>, size: u64, kind: Kind) -> Node {
        Node { name: name.into(), path, size, kind, thresh: 0, children: Vec::new() }
    }

    fn get(&self, rel: &[usize]) -> Option<&Node> {
        rel.iter().try_fold(self, |n, &i| n.children.get(i))
    }

    fn has_smaller(&self) -> bool {
        self.children.last().is_some_and(|c| matches!(c.kind, Kind::Smaller(_)))
    }

    fn real(&self) -> bool {
        self.path.is_some() && !matches!(self.kind, Kind::Smaller(_) | Kind::Other)
    }

    fn icon(&self) -> &'static str {
        match self.kind {
            Kind::Dir => ic::FOLDER,
            Kind::File => ic::FILE,
            Kind::Package => ic::PACKAGE,
            Kind::Smaller(_) => ic::DOTS_THREE,
            Kind::Other => ic::HARD_DRIVES,
        }
    }
}

fn on_disk(m: &fs::Metadata) -> u64 {
    m.blocks() * 512
}

fn name_of(p: &Path) -> String {
    p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| p.display().to_string())
}

fn is_package(name: &str) -> bool {
    name.rsplit_once('.').is_some_and(|(stem, ext)| !stem.is_empty() && PACKAGES.contains(&ext.to_ascii_lowercase().as_str()))
}

fn ls(p: &Path) -> Vec<PathBuf> {
    fs::read_dir(p).map(|rd| rd.flatten().map(|e| e.path()).collect()).unwrap_or_default()
}

/// Threshold used when a pruned folder is opened and rescanned in detail.
fn fine_thresh(size: u64) -> u64 {
    (size / 1000).max(64 * 1024)
}

/// Builds a directory node: sorts children and merges the ones below `thresh`.
fn finish(name: String, path: Option<PathBuf>, size: u64, mut kids: Vec<Node>, thresh: u64) -> Node {
    kids.sort_by(|a, b| b.size.cmp(&a.size));
    let cut = kids.iter().position(|k| k.size < thresh).unwrap_or(kids.len());
    if kids.len() - cut > 1 {
        let rest = kids.split_off(cut);
        let total: u64 = rest.iter().map(|k| k.size).sum();
        if total > 0 {
            let n = rest.len() as u32;
            kids.push(Node::leaf(format!("{n} smaller items"), None, total, Kind::Smaller(n)));
        }
    }
    Node { name, path, size, kind: Kind::Dir, thresh, children: kids }
}

#[derive(Default)]
struct Progress {
    current: Mutex<String>,
    files: AtomicU64,
    done: Mutex<Option<Outcome>>,
}

enum Outcome {
    Full(Node, Changes),
    Sub(Node),
}

struct Walker<'a> {
    prog: &'a Progress,
    last: Instant,
    thresh: u64,
    dev: u64,
    home: &'a Path,
    snap: Vec<(String, u64)>,
}

impl Walker<'_> {
    /// `depth` is the depth below home when recording a snapshot, else None.
    fn entry(&mut self, path: PathBuf, m: &fs::Metadata, depth: Option<usize>) -> Option<Node> {
        if m.dev() != self.dev {
            return None; // another volume mounted inside
        }
        self.prog.files.fetch_add(1, Ordering::Relaxed);
        let name = name_of(&path);
        if !m.is_dir() {
            // Files and symlinks (never followed).
            return Some(Node::leaf(name, Some(path), on_disk(m), Kind::File));
        }
        let node = if is_package(&name) {
            let size = scan::dir_size(&path);
            Node::leaf(name, Some(path), size, Kind::Package)
        } else {
            self.dir(path, name, m, depth)
        };
        if let (Some(d), Some(p)) = (depth, &node.path) {
            if d <= 3 && node.size >= MB {
                if let Ok(rel) = p.strip_prefix(self.home) {
                    let rel = rel.to_string_lossy();
                    if !rel.contains(['\t', '\n']) {
                        self.snap.push((rel.into_owned(), node.size));
                    }
                }
            }
        }
        Some(node)
    }

    fn dir(&mut self, path: PathBuf, name: String, m: &fs::Metadata, depth: Option<usize>) -> Node {
        if self.last.elapsed() > Duration::from_millis(100) {
            self.last = Instant::now();
            *self.prog.current.lock().unwrap() = short(&path);
        }
        let mut size = on_disk(m);
        let mut kids = Vec::new();
        if let Ok(rd) = fs::read_dir(&path) {
            for e in rd.flatten() {
                // DirEntry::metadata does not follow symlinks.
                let Ok(cm) = e.metadata() else { continue };
                if let Some(n) = self.entry(e.path(), &cm, depth.map(|d| d + 1)) {
                    size += n.size;
                    kids.push(n);
                }
            }
        }
        finish(name, Some(path), size, kids, self.thresh)
    }
}

/// Walks several groups of roots with a pool of threads. Returns the nodes per
/// group and the snapshot entries (only group 0 is recorded, when `record`).
fn scan_groups(groups: Vec<Vec<PathBuf>>, thresh: u64, prog: &Progress, home: &Path, record: bool) -> (Vec<Vec<Node>>, Vec<(String, u64)>) {
    let n_groups = groups.len();
    let queue: Mutex<Vec<(usize, PathBuf)>> =
        Mutex::new(groups.into_iter().enumerate().flat_map(|(g, v)| v.into_iter().map(move |p| (g, p))).collect());
    let out: Mutex<Vec<(usize, Node)>> = Mutex::new(Vec::new());
    let snap: Mutex<Vec<(String, u64)>> = Mutex::new(Vec::new());
    let workers = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(2, 8);
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| {
                let mut w = Walker { prog, last: Instant::now(), thresh, dev: 0, home, snap: Vec::new() };
                loop {
                    let Some((g, p)) = queue.lock().unwrap().pop() else { break };
                    let Ok(m) = fs::symlink_metadata(&p) else { continue };
                    w.dev = m.dev();
                    let depth = (record && g == 0).then_some(1);
                    if let Some(n) = w.entry(p, &m, depth) {
                        out.lock().unwrap().push((g, n));
                    }
                }
                snap.lock().unwrap().append(&mut w.snap);
            });
        }
    });
    let mut res: Vec<Vec<Node>> = (0..n_groups).map(|_| Vec::new()).collect();
    for (g, n) in out.into_inner().unwrap() {
        res[g].push(n);
    }
    (res, snap.into_inner().unwrap())
}

fn dir_node(name: &str, path: PathBuf, kids: Vec<Node>, thresh: u64) -> Node {
    let own = fs::symlink_metadata(&path).map(|m| on_disk(&m)).unwrap_or(0);
    let size = own + kids.iter().map(|k| k.size).sum::<u64>();
    finish(name.to_string(), Some(path), size, kids, thresh)
}

fn full_scan(prog: &Progress, disk: Option<(u64, u64)>) -> Outcome {
    let home = scan::home();
    let apps = PathBuf::from("/Applications");
    let (mut groups, snap) = scan_groups(vec![ls(&home), ls(&apps)], FIRST_THRESH, prog, &home, true);
    let apps_node = dir_node("Applications", apps, groups.pop().unwrap_or_default(), FIRST_THRESH);
    let home_node = dir_node("Home", home, groups.pop().unwrap_or_default(), FIRST_THRESH);
    let mut kids = vec![home_node, apps_node];
    let counted: u64 = kids.iter().map(|k| k.size).sum();
    if let Some((free, total)) = disk {
        let other = total.saturating_sub(free).saturating_sub(counted);
        if other > 0 {
            kids.push(Node::leaf("System & other", None, other, Kind::Other));
        }
    }
    kids.sort_by(|a, b| b.size.cmp(&a.size));
    let size = kids.iter().map(|k| k.size).sum();
    let root = Node { name: "Macintosh HD".into(), path: None, size, kind: Kind::Dir, thresh: FIRST_THRESH, children: kids };

    // Snapshot + compare with the previous one.
    let dir = snapshot_dir();
    let now = unix_now();
    let new: HashMap<String, u64> = snap.into_iter().collect();
    let prev = list_snapshots(&dir).pop().and_then(|(t, p)| Some((t, parse_tsv(&fs::read_to_string(p).ok()?))));
    let _ = save_snapshot(&dir, now, &new, KEEP_SNAPSHOTS);
    let changes = match prev {
        Some((t, old)) => Changes::between(t, now, &old, &new),
        None => Changes { since: None, latest: now, grew: vec![], shrank: vec![] },
    };
    Outcome::Full(root, changes)
}

fn sub_scan(prog: &Progress, name: &str, path: PathBuf, thresh: u64) -> Outcome {
    let home = scan::home();
    let (mut groups, _) = scan_groups(vec![ls(&path)], thresh, prog, &home, false);
    Outcome::Sub(dir_node(name, path, groups.pop().unwrap_or_default(), thresh))
}

/// Removes deleted paths from the tree and shrinks their ancestors. Returns bytes removed.
fn drop_removed(n: &mut Node, removed: &HashSet<PathBuf>) -> u64 {
    let mut freed = 0;
    n.children.retain(|c| {
        let gone = c.path.as_ref().is_some_and(|p| removed.contains(p));
        if gone {
            freed += c.size;
        }
        !gone
    });
    for c in &mut n.children {
        freed += drop_removed(c, removed);
    }
    n.size = n.size.saturating_sub(freed);
    freed
}

/// Folders that must never be offered for deletion from the map.
fn protected(p: &Path) -> bool {
    let h = scan::home();
    let keep = ["Library", "Library/Application Support", "Library/Containers", "Library/Group Containers", "Library/Preferences", "Library/Mobile Documents"];
    p == h || p == Path::new("/Applications") || p.parent().map_or(true, |x| x == Path::new("/")) || keep.iter().any(|k| p == h.join(k))
}

// ---------- snapshots ----------

#[derive(Debug, Clone, PartialEq)]
pub struct Changes {
    /// Time of the snapshot we compare against (None = only one snapshot so far).
    since: Option<u64>,
    latest: u64,
    grew: Vec<(String, i64)>,
    shrank: Vec<(String, i64)>,
}

impl Changes {
    fn between(since: u64, latest: u64, old: &HashMap<String, u64>, new: &HashMap<String, u64>) -> Changes {
        let (mut grew, mut shrank) = diff(old, new, MIN_CHANGE);
        grew.truncate(8);
        shrank.truncate(8);
        Changes { since: Some(since), latest, grew, shrank }
    }
}

fn snapshot_dir() -> PathBuf {
    scan::home().join("Library/Application Support/CleanYou/snapshots")
}

fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn to_tsv(entries: &HashMap<String, u64>) -> String {
    let mut v: Vec<_> = entries.iter().collect();
    v.sort();
    v.into_iter().map(|(p, s)| format!("{s}\t{p}\n")).collect()
}

fn parse_tsv(s: &str) -> HashMap<String, u64> {
    s.lines()
        .filter_map(|l| {
            let (size, path) = l.split_once('\t')?;
            Some((path.to_string(), size.trim().parse().ok()?))
        })
        .collect()
}

/// (folders that grew, largest first; folders that shrank, largest first).
/// A folder is left out when one of its subfolders explains most of its change,
/// so the list points at the specific folder.
fn diff(old: &HashMap<String, u64>, new: &HashMap<String, u64>, min: u64) -> (Vec<(String, i64)>, Vec<(String, i64)>) {
    let mut delta: HashMap<&str, i64> = HashMap::new();
    for k in old.keys().chain(new.keys()) {
        let d = *new.get(k).unwrap_or(&0) as i64 - *old.get(k).unwrap_or(&0) as i64;
        delta.insert(k.as_str(), d);
    }
    let mut explained: HashSet<&str> = HashSet::new();
    for (&k, &d) in &delta {
        if d.unsigned_abs() < min {
            continue;
        }
        let mut p = k;
        while let Some((parent, _)) = p.rsplit_once('/') {
            if let Some(&pd) = delta.get(parent) {
                if pd.signum() == d.signum() && d.unsigned_abs() * 10 >= pd.unsigned_abs() * 8 {
                    explained.insert(parent);
                }
            }
            p = parent;
        }
    }
    let keep = |(k, d): (&&str, &i64)| (d.unsigned_abs() >= min && !explained.contains(*k)).then(|| (k.to_string(), *d));
    let mut grew: Vec<_> = delta.iter().filter(|(_, d)| **d > 0).filter_map(keep).collect();
    let mut shrank: Vec<_> = delta.iter().filter(|(_, d)| **d < 0).filter_map(keep).collect();
    grew.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    shrank.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0)));
    (grew, shrank)
}

/// Snapshot files in `dir`, oldest first.
fn list_snapshots(dir: &Path) -> Vec<(u64, PathBuf)> {
    let mut v: Vec<(u64, PathBuf)> = ls(dir)
        .into_iter()
        .filter(|p| p.extension().is_some_and(|e| e == "tsv"))
        .filter_map(|p| Some((p.file_stem()?.to_str()?.parse().ok()?, p)))
        .collect();
    v.sort();
    v
}

fn save_snapshot(dir: &Path, t: u64, entries: &HashMap<String, u64>, keep: usize) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;
    fs::write(dir.join(format!("{t}.tsv")), to_tsv(entries))?;
    let all = list_snapshots(dir);
    for (_, p) in all.iter().take(all.len().saturating_sub(keep)) {
        let _ = fs::remove_file(p);
    }
    Ok(())
}

/// Changes between the two most recent snapshots on disk.
fn load_changes(dir: &Path) -> Option<Changes> {
    let mut all = list_snapshots(dir);
    let (latest, lp) = all.pop()?;
    let Some((since, sp)) = all.pop() else {
        return Some(Changes { since: None, latest, grew: vec![], shrank: vec![] });
    };
    let read = |p: &Path| parse_tsv(&fs::read_to_string(p).unwrap_or_default());
    Some(Changes::between(since, latest, &read(&sp), &read(&lp)))
}

fn ago(secs: u64) -> String {
    let plural = |n: u64, unit: &str| format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" });
    match secs {
        0..=59 => "just now".into(),
        60..=3599 => format!("{} min ago", secs / 60),
        3600..=86_399 => plural(secs / 3600, "hour"),
        _ => plural(secs / 86_400, "day"),
    }
}

// ---------- sunburst geometry ----------

struct Seg {
    /// Child indices from the focused node.
    rel: Vec<usize>,
    depth: usize,
    a0: f32,
    a1: f32,
    color: Color32,
}

fn shade(base: Color32, depth: usize, odd: bool) -> Color32 {
    let light = Color32::from_rgb(210, 210, 216);
    let c = base.lerp_to_gamma(light, (0.2 * (depth as f32 - 1.0)).min(0.6));
    if odd {
        c.lerp_to_gamma(BG, 0.1)
    } else {
        c
    }
}

fn top_color(n: &Node, i: usize) -> Color32 {
    match n.kind {
        Kind::Smaller(_) => GRAY_SMALL,
        Kind::Other => GRAY_OTHER,
        _ => PALETTE[i % PALETTE.len()],
    }
}

/// Angular segments for `RINGS` levels below `focus`. Angles in [0, TAU), clockwise from 12 o'clock.
/// The first ring gets one color per folder. When one folder takes up most of the chart (Home on the
/// whole disk, say), its children get their own colors too, so the map isn't one big blue blob.
fn layout(focus: &Node) -> Vec<Seg> {
    #[allow(clippy::too_many_arguments)]
    fn rec(n: &Node, rel: &mut Vec<usize>, depth: usize, a0: f32, span: f32, base: Option<(Color32, usize)>, next: &mut usize, out: &mut Vec<Seg>) {
        if depth > RINGS || n.size == 0 {
            return;
        }
        let dominant = depth == 2 && span >= std::f32::consts::PI;
        let mut a = a0;
        for (i, c) in n.children.iter().enumerate() {
            let s = (span as f64 * c.size as f64 / n.size as f64) as f32;
            if s >= 0.004 {
                let (color, own) = match (base, c.kind) {
                    (_, Kind::Smaller(_)) => (GRAY_SMALL, false),
                    (_, Kind::Other) => (GRAY_OTHER, false),
                    (None, _) => (top_color(c, i), true),
                    (Some(_), _) if dominant => {
                        *next += 1;
                        (PALETTE[*next % PALETTE.len()], true)
                    }
                    (Some((b, d)), _) => (shade(b, depth + 1 - d, i % 2 == 1), false),
                };
                rel.push(i);
                out.push(Seg { rel: rel.clone(), depth, a0: a, a1: a + s, color });
                let child_base = if own { Some((color, depth)) } else { base };
                rec(c, rel, depth + 1, a, s, child_base, next, out);
                rel.pop();
            }
            a += s;
        }
    }
    let mut out = Vec::new();
    // Second-ring colors continue after the first ring's, so neighbours differ.
    let mut next = focus.children.len().min(PALETTE.len());
    rec(focus, &mut Vec::new(), 1, 0.0, TAU, None, &mut next, &mut out);
    out
}

#[derive(Clone, Copy)]
struct Geo {
    c: Pos2,
    r0: f32,
    w: f32,
}

impl Geo {
    fn new(rect: Rect) -> Geo {
        let r = rect.width().min(rect.height()) / 2.0 - 4.0;
        let r0 = r * 0.3;
        Geo { c: rect.center(), r0, w: (r - r0) / RINGS as f32 }
    }

    fn radii(&self, depth: usize) -> (f32, f32) {
        let i = self.r0 + (depth - 1) as f32 * self.w;
        (i + 1.0, i + self.w - 1.0)
    }
}

enum Hit {
    Center,
    Seg(usize),
}

fn hit(segs: &[Seg], g: Geo, pos: Pos2) -> Option<Hit> {
    let d = pos - g.c;
    let r = d.length();
    if r < g.r0 {
        return Some(Hit::Center);
    }
    let depth = ((r - g.r0) / g.w) as usize + 1;
    if depth > RINGS {
        return None;
    }
    let ang = (d.y.atan2(d.x) + FRAC_PI_2).rem_euclid(TAU);
    segs.iter().position(|s| s.depth == depth && ang >= s.a0 && ang < s.a1).map(Hit::Seg)
}

/// Adds an annular sector as a triangle strip (non-convex, so not a Shape::convex_polygon).
fn add_arc(mesh: &mut Mesh, c: Pos2, r_in: f32, r_out: f32, a0: f32, a1: f32, color: Color32) {
    let gap = 0.75 / r_out;
    let (a0, a1) = if a1 - a0 > 4.0 * gap { (a0 + gap, a1 - gap) } else { (a0, a1) };
    let n = (((a1 - a0) * r_out / 4.0).ceil() as u32).clamp(1, 512);
    let base = mesh.vertices.len() as u32;
    for k in 0..=n {
        let a = a0 + (a1 - a0) * k as f32 / n as f32 - FRAC_PI_2;
        let d = vec2(a.cos(), a.sin());
        mesh.colored_vertex(c + d * r_in, color);
        mesh.colored_vertex(c + d * r_out, color);
    }
    for k in 0..n {
        let i = base + 2 * k;
        mesh.add_triangle(i, i + 1, i + 2);
        mesh.add_triangle(i + 1, i + 3, i + 2);
    }
}

struct Cache {
    key: (u64, Vec<usize>, [i32; 3]),
    segs: Vec<Seg>,
    mesh: Mesh,
}

// ---------- page state ----------

struct Task {
    prog: Arc<Progress>,
    /// None = full scan; Some = refining this node (index path + its path).
    target: Option<(Vec<usize>, PathBuf)>,
    started: Instant,
}

#[derive(Default)]
pub struct State {
    root: Option<Node>,
    focus: Vec<usize>,
    task: Option<Task>,
    /// Bumped whenever the tree changes, to invalidate the cached mesh.
    gen: u64,
    removed_seen: usize,
    cache: Option<Cache>,
    /// Focus child hovered in the list / chart last frame (for cross-highlighting).
    list_hover: Option<usize>,
    chart_hover: Option<usize>,
    changes: Option<Changes>,
    loaded: bool,
}

enum Act {
    Scan,
    Focus(Vec<usize>),
    Reveal(PathBuf),
    Trash(PathBuf, String, u64),
}

impl State {
    fn focused(&self) -> Option<&Node> {
        self.root.as_ref()?.get(&self.focus)
    }

    fn apply_removed(&mut self, removed: &HashSet<PathBuf>) {
        let Some(root) = &mut self.root else { return };
        // Remember the focus by name, since indices shift when children go away.
        let mut names = Vec::new();
        let mut n: &Node = root;
        for &i in &self.focus {
            n = &n.children[i];
            names.push(n.name.clone());
        }
        if drop_removed(root, removed) == 0 {
            return;
        }
        self.focus.clear();
        let mut n: &Node = root;
        for name in names {
            match n.children.iter().position(|c| c.name == name) {
                Some(i) if !n.children[i].children.is_empty() => {
                    self.focus.push(i);
                    n = &n.children[i];
                }
                _ => break,
            }
        }
        self.gen += 1;
    }

    fn replace(&mut self, idx: &[usize], path: &Path, mut new: Node) {
        new.thresh = 0; // refined: never rescan this level again, even if its size drifts
        let Some(root) = &mut self.root else { return };
        let Some(old) = root.get(idx) else { return };
        if old.path.as_deref() != Some(path) {
            return; // tree changed meanwhile
        }
        let (old_size, new_size) = (old.size, new.size);
        let fix = |s: &mut u64| *s = (*s + new_size).saturating_sub(old_size);
        let mut n = root;
        fix(&mut n.size);
        for &i in idx {
            n = &mut n.children[i];
            fix(&mut n.size);
        }
        *n = new;
        self.gen += 1;
    }

    fn poll(&mut self, removed: &HashSet<PathBuf>) {
        if !self.loaded {
            self.loaded = true;
            self.changes = load_changes(&snapshot_dir());
        }
        let done = self.task.as_ref().and_then(|t| t.prog.done.lock().unwrap().take());
        if let Some(out) = done {
            let task = self.task.take().unwrap();
            match out {
                Outcome::Full(root, changes) => {
                    self.root = Some(root);
                    self.focus.clear();
                    self.changes = Some(changes);
                    self.gen += 1;
                    self.removed_seen = 0;
                }
                Outcome::Sub(node) => {
                    if let Some((idx, path)) = task.target {
                        self.replace(&idx, &path, node);
                    }
                }
            }
        }
        if removed.len() != self.removed_seen {
            self.removed_seen = removed.len();
            self.apply_removed(removed);
        }
        // Lazily refine a focused folder whose contents were merged on the first pass.
        if self.task.is_none() {
            if let Some(n) = self.focused() {
                if let (true, Some(p)) = (n.has_smaller() && n.kind == Kind::Dir && n.thresh > fine_thresh(n.size), &n.path) {
                    let (name, path, thresh) = (n.name.clone(), p.clone(), fine_thresh(n.size));
                    let prog = Arc::new(Progress::default());
                    let p2 = prog.clone();
                    let target = Some((self.focus.clone(), path.clone()));
                    std::thread::spawn(move || {
                        let out = sub_scan(&p2, &name, path, thresh);
                        *p2.done.lock().unwrap() = Some(out);
                    });
                    self.task = Some(Task { prog, target, started: Instant::now() });
                }
            }
        }
    }

    fn start_full(&mut self) {
        if self.task.is_some() {
            return;
        }
        let prog = Arc::new(Progress::default());
        let p2 = prog.clone();
        std::thread::spawn(move || {
            let out = full_scan(&p2, system::disk());
            *p2.done.lock().unwrap() = Some(out);
        });
        self.task = Some(Task { prog, target: None, started: Instant::now() });
    }

    fn cache(&mut self, g: Geo) {
        let key = (self.gen, self.focus.clone(), [g.c.x.round() as i32, g.c.y.round() as i32, (g.w * 10.0) as i32]);
        if self.cache.as_ref().is_some_and(|c| c.key == key) {
            return;
        }
        let Some(focus) = self.focused() else { return };
        let segs = layout(focus);
        let mut mesh = Mesh::default();
        for s in &segs {
            let (ri, ro) = g.radii(s.depth);
            add_arc(&mut mesh, g.c, ri, ro, s.a0, s.a1, s.color);
        }
        self.cache = Some(Cache { key, segs, mesh });
    }
}

fn pct(part: u64, whole: u64) -> f32 {
    if whole == 0 {
        0.0
    } else {
        part as f32 / whole as f32
    }
}

impl App {
    pub(crate) fn diskmap_page(&mut self, ui: &mut Ui) {
        self.diskmap.poll(&self.removed);
        let st = &self.diskmap;
        let busy_full = st.task.as_ref().is_some_and(|t| t.target.is_none());
        if st.task.is_some() {
            ui.ctx().request_repaint_after(Duration::from_millis(150));
        }
        let sub = match (&st.task, &st.changes) {
            (Some(t), _) if t.target.is_none() => "Mapping your disk…".to_string(),
            (_, Some(c)) if st.root.is_some() => format!("Scanned {}. Click a ring to zoom in, the center to go back.", ago(unix_now().saturating_sub(c.latest))),
            _ => "See what fills your disk, and what grew since last time.".to_string(),
        };
        let mut act: Option<Act> = None;
        let has_root = st.root.is_some();
        header(ui, ic::CHART_DONUT, ACCENT, "Disk Map", &sub, |ui| {
            let label = format!("{}  {}", if has_root { ic::ARROWS_CLOCKWISE } else { ic::MAGNIFYING_GLASS }, if has_root { "Rescan" } else { "Scan" });
            if primary(ui, !busy_full, label, ACCENT).clicked() {
                act = Some(Act::Scan);
            }
        });

        egui::ScrollArea::vertical().id_salt("diskmap_page").auto_shrink([false; 2]).show(ui, |ui| {
            if busy_full {
                self.scan_card(ui);
            } else if self.diskmap.root.is_some() {
                let w = ui.available_width();
                if w >= 760.0 {
                    let chart = (w * 0.5).clamp(320.0, 560.0);
                    ui.horizontal_top(|ui| {
                        ui.vertical(|ui| {
                            ui.set_width(chart);
                            card(ui, |ui| self.sunburst(ui, chart - 38.0, &mut act));
                        });
                        ui.add_space(12.0);
                        ui.vertical(|ui| {
                            card(ui, |ui| self.side_list(ui, chart - 42.0, &mut act));
                        });
                    });
                } else {
                    card(ui, |ui| self.sunburst(ui, (w - 38.0).min(460.0), &mut act));
                    ui.add_space(12.0);
                    card(ui, |ui| self.side_list(ui, 360.0, &mut act));
                }
            } else {
                card(ui, |ui| {
                    ui.vertical_centered(|ui| {
                        ui.add_space(24.0);
                        ui.label(RichText::new(ic::CHART_DONUT).size(56.0).color(ACCENT));
                        ui.add_space(6.0);
                        ui.label(RichText::new("See where your space went").size(18.0).strong().color(Color32::WHITE));
                        ui.label(RichText::new("Maps your home folder and apps into an interactive chart. Nothing is changed.").color(DIM));
                        ui.add_space(12.0);
                        if primary(ui, true, format!("{}  Scan", ic::MAGNIFYING_GLASS), ACCENT).clicked() {
                            act = Some(Act::Scan);
                        }
                        ui.add_space(24.0);
                    });
                });
            }
            ui.add_space(12.0);
            self.changes_card(ui);
        });

        match act {
            Some(Act::Scan) => self.diskmap.start_full(),
            Some(Act::Focus(f)) => {
                self.diskmap.focus = f;
                self.diskmap.list_hover = None;
            }
            Some(Act::Reveal(p)) => system::reveal(&p),
            Some(Act::Trash(p, label, size)) => {
                self.ask_delete_paths(format!("Move “{label}” to the Trash?"), vec![(p, label, size)], false);
            }
            None => {}
        }
    }

    fn scan_card(&self, ui: &mut Ui) {
        let Some(t) = &self.diskmap.task else { return };
        card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(16.0).color(ACCENT));
                ui.label(RichText::new("Mapping your disk…").strong().color(Color32::WHITE));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let files = t.prog.files.load(Ordering::Relaxed);
                    ui.label(RichText::new(format!("{files} items · {}s", t.started.elapsed().as_secs())).color(DIM));
                });
            });
            ui.add_space(8.0);
            indeterminate(ui, ACCENT);
            ui.add_space(6.0);
            let cur = t.prog.current.lock().unwrap().clone();
            ui.add(Label::new(RichText::new(cur).small().color(DIM)).truncate());
        });
    }

    fn sunburst(&mut self, ui: &mut Ui, size: f32, act: &mut Option<Act>) {
        let (rect, resp) = ui.allocate_exact_size(vec2(size, size), Sense::click());
        let g = Geo::new(rect);
        let st = &mut self.diskmap;
        st.cache(g);
        let (Some(cache), Some(focus)) = (&st.cache, st.root.as_ref().and_then(|r| r.get(&st.focus))) else { return };
        let p = ui.painter_at(rect.expand(2.0));
        p.add(Shape::mesh(cache.mesh.clone()));

        let hovered = resp.hover_pos().and_then(|pos| hit(&cache.segs, g, pos));
        let hl = match &hovered {
            Some(Hit::Seg(i)) => Some(*i),
            _ => st.list_hover.and_then(|li| cache.segs.iter().position(|s| s.rel.len() == 1 && s.rel[0] == li)),
        };
        // Focus the highlighted slice: dim everything else, keep its parents and contents in color.
        if let Some(i) = hl {
            let s = &cache.segs[i];
            let mut m = Mesh::default();
            add_arc(&mut m, g.c, g.r0, g.r0 + g.w * RINGS as f32, 0.0, TAU - 0.0001, Color32::from_black_alpha(150));
            for o in &cache.segs {
                let related = o.rel.starts_with(&s.rel) || s.rel.starts_with(&o.rel);
                if related {
                    let (ri, ro) = g.radii(o.depth);
                    let c = if o.rel == s.rel { o.color.lerp_to_gamma(Color32::WHITE, 0.22) } else { o.color };
                    add_arc(&mut m, g.c, ri, ro, o.a0, o.a1, c);
                }
            }
            p.add(Shape::mesh(m));
            // Outline the hovered slice.
            let (ri, ro) = g.radii(s.depth);
            let n = 48;
            let mut pts = Vec::with_capacity(2 * n + 2);
            for k in 0..=n {
                let a = s.a0 + (s.a1 - s.a0) * k as f32 / n as f32 - FRAC_PI_2;
                pts.push(g.c + vec2(a.cos(), a.sin()) * (ro + 1.0));
            }
            for k in (0..=n).rev() {
                let a = s.a0 + (s.a1 - s.a0) * k as f32 / n as f32 - FRAC_PI_2;
                pts.push(g.c + vec2(a.cos(), a.sin()) * (ri - 1.0));
            }
            p.add(Shape::closed_line(pts, Stroke::new(1.5_f32, Color32::WHITE)));
        }

        // Names on slices big enough to hold them.
        if hl.is_none() {
            for s in cache.segs.iter().filter(|s| s.depth <= 2) {
                let (ri, ro) = g.radii(s.depth);
                let mid_r = (ri + ro) / 2.0;
                let arc = (s.a1 - s.a0) * mid_r;
                if arc < 64.0 || g.w < 22.0 {
                    continue;
                }
                let Some(n) = focus.get(&s.rel) else { continue };
                if !n.real() {
                    continue;
                }
                let a = (s.a0 + s.a1) / 2.0 - FRAC_PI_2;
                let at = g.c + vec2(a.cos(), a.sin()) * mid_r;
                let gal = truncated(ui, &n.name, 11.5, Color32::WHITE, (arc * 0.8).min(g.w * 1.6));
                let pos = at - gal.size() / 2.0;
                let pad = Rect::from_min_size(pos, gal.size()).expand2(vec2(4.0, 1.0));
                p.rect_filled(pad, 4.0, Color32::from_black_alpha(120));
                p.galley(pos, gal, Color32::WHITE);
            }
        }

        // Center: what's hovered, or the focused folder (click to go up).
        let center_hot = matches!(hovered, Some(Hit::Center)) && !st.focus.is_empty();
        p.circle_filled(g.c, g.r0 - 3.0, if center_hot { Color32::from_rgb(40, 40, 45) } else { Color32::from_rgb(28, 28, 31) });
        p.circle_stroke(g.c, g.r0 - 3.0, Stroke::new(1.0_f32, BORDER));
        let shown = hl.and_then(|i| {
            let rel = &cache.segs[i].rel;
            Some((focus.get(rel)?, focus.get(&rel[..rel.len() - 1])?))
        });
        let big = FontId::proportional((g.r0 * 0.24).clamp(14.0, 22.0));
        match shown {
            Some((n, parent)) => {
                let name = truncated(ui, &n.name, 12.5, Color32::WHITE, g.r0 * 1.6);
                p.galley(g.c + vec2(-name.size().x / 2.0, -28.0), name, Color32::WHITE);
                p.text(g.c + vec2(0.0, 1.0), Align2::CENTER_CENTER, human(n.size), big, Color32::WHITE);
                let pc = format!("{:.1}% of {}", pct(n.size, parent.size) * 100.0, parent.name);
                let pc = truncated(ui, &pc, 11.0, DIM, g.r0 * 1.6);
                p.galley(g.c + vec2(-pc.size().x / 2.0, 18.0), pc, DIM);
            }
            None => {
                let name = truncated(ui, &focus.name, 12.5, DIM, g.r0 * 1.6);
                p.galley(g.c + vec2(-name.size().x / 2.0, -26.0), name, DIM);
                p.text(g.c + vec2(0.0, 2.0), Align2::CENTER_CENTER, human(focus.size), big, Color32::WHITE);
                if !st.focus.is_empty() {
                    let c = if center_hot { Color32::WHITE } else { DIM };
                    p.text(g.c + vec2(0.0, 24.0), Align2::CENTER_CENTER, format!("{} Up", ic::ARROW_UP), FontId::proportional(11.5), c);
                }
            }
        }

        st.chart_hover = match &hovered {
            Some(Hit::Seg(i)) => cache.segs[*i].rel.first().copied(),
            _ => None,
        };
        let mut click: Option<Act> = None;
        if resp.clicked() {
            match &hovered {
                Some(Hit::Center) if !st.focus.is_empty() => {
                    let mut f = st.focus.clone();
                    f.pop();
                    click = Some(Act::Focus(f));
                }
                Some(Hit::Seg(i)) => {
                    let rel = &cache.segs[*i].rel;
                    if focus.get(rel).is_some_and(|n| !n.children.is_empty()) {
                        click = Some(Act::Focus(st.focus.iter().chain(rel).copied().collect()));
                    }
                }
                _ => {}
            }
        }
        let pointer = match &hovered {
            Some(Hit::Center) if !st.focus.is_empty() => true,
            Some(Hit::Seg(i)) => focus.get(&cache.segs[*i].rel).is_some_and(|n| !n.children.is_empty()),
            _ => false,
        };
        if pointer {
            resp.on_hover_cursor(egui::CursorIcon::PointingHand);
        }
        if click.is_some() {
            *act = click;
        }

        // Info bar under the chart: full details of what's under the pointer.
        ui.add_space(8.0);
        let (bar, _) = ui.allocate_exact_size(vec2(size, 40.0), Sense::hover());
        let bp = ui.painter();
        bp.rect_filled(bar, 9.0, Color32::from_rgb(24, 24, 27));
        match shown {
            Some((n, _)) => {
                let s = &cache.segs[hl.unwrap()];
                bp.circle_filled(bar.left_center() + vec2(14.0, 0.0), 5.0, s.color);
                let size_g = bp.layout_no_wrap(human(n.size), FontId::proportional(13.5), Color32::WHITE);
                let right = bar.right() - 12.0 - size_g.size().x;
                bp.galley(pos2(right, bar.center().y - size_g.size().y / 2.0), size_g, Color32::WHITE);
                let w = right - (bar.left() + 28.0) - 10.0;
                let title = truncated(ui, &format!("{}  {}", n.icon(), n.name), 12.5, Color32::WHITE, w);
                ui.painter().galley(pos2(bar.left() + 28.0, bar.top() + 4.0), title, Color32::WHITE);
                let sub = match (&n.path, n.kind) {
                    (_, Kind::Smaller(k)) => format!("{k} items too small to show separately"),
                    (Some(path), _) if !n.children.is_empty() => format!("{} · click to open", short(path)),
                    (Some(path), _) => short(path),
                    (None, _) => "Space macOS and other users use".to_string(),
                };
                let sub = truncated(ui, &sub, 11.0, DIM, w);
                ui.painter().galley(pos2(bar.left() + 28.0, bar.top() + 22.0), sub, DIM);
            }
            None => {
                let hint = format!("{}  Point at a ring to see what it is · click to zoom in · click the center to go back", ic::CURSOR_CLICK);
                let g = truncated(ui, &hint, 11.5, DIM, bar.width() - 24.0);
                bp.galley(bar.center() - g.size() / 2.0, g, DIM);
            }
        }

        // Fine-detail rescan in progress.
        if let Some(t) = &st.task {
            if t.target.is_some() {
                p.text(rect.left_top() + vec2(2.0, 2.0), Align2::LEFT_TOP, format!("{} Refining…", ic::CIRCLE_NOTCH), FontId::proportional(11.5), DIM);
            }
        }
    }

    fn side_list(&mut self, ui: &mut Ui, height: f32, act: &mut Option<Act>) {
        let st = &mut self.diskmap;
        let Some(root) = &st.root else { return };
        let Some(focus) = root.get(&st.focus) else { return };

        // Breadcrumb.
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            let mut n = root;
            for d in 0..=st.focus.len() {
                if d > 0 {
                    ui.label(RichText::new(ic::CARET_RIGHT).small().color(DIM));
                    n = &n.children[st.focus[d - 1]];
                }
                let last = d == st.focus.len();
                let icon = if d == 0 { ic::HARD_DRIVE } else { "" };
                let text = RichText::new(format!("{icon}{}{}", if d == 0 { " " } else { "" }, n.name));
                let text = if last { text.strong().color(Color32::WHITE) } else { text.color(DIM) };
                if ui.add(Button::new(text).frame(false)).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() && !last {
                    *act = Some(Act::Focus(st.focus[..d].to_vec()));
                }
            }
        });
        ui.add_space(4.0);
        ui.separator();

        let row_h = 42.0;
        let mut list_hover = None;
        let top = focus.children.first().map_or(1, |c| c.size).max(1);
        let base_color = |i: usize| -> Color32 {
            // Same colors as the first ring.
            top_color(&focus.children[i], i)
        };
        egui::ScrollArea::vertical().id_salt("diskmap_list").max_height(height).auto_shrink([false, true]).show_rows(
            ui,
            row_h,
            focus.children.len(),
            |ui, range| {
                for i in range {
                    let c = &focus.children[i];
                    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), row_h), Sense::click());
                    let drill = !c.children.is_empty();
                    let hot = resp.hovered() || st.chart_hover == Some(i);
                    if resp.hovered() {
                        list_hover = Some(i);
                    }
                    let p = ui.painter();
                    if hot {
                        p.rect_filled(rect, 8.0, Color32::from_rgb(38, 38, 43));
                    }
                    let color = base_color(i);
                    p.circle_filled(rect.left_center() + vec2(12.0, -5.0), 5.0, color);

                    let buttons = if c.real() { 66.0 } else { 0.0 };
                    let size_x = rect.right() - buttons - 8.0;
                    p.text(pos2(size_x, rect.center().y - 5.0), Align2::RIGHT_CENTER, human(c.size), FontId::proportional(13.5), Color32::WHITE);
                    let share = format!("{:.0}%", pct(c.size, focus.size) * 100.0);
                    p.text(pos2(size_x - 76.0, rect.center().y - 5.0), Align2::RIGHT_CENTER, share, FontId::proportional(11.5), DIM);
                    let name_w = size_x - 112.0 - (rect.left() + 26.0);
                    let label = format!("{}  {}", c.icon(), c.name);
                    let g = truncated(ui, &label, 13.5, Color32::WHITE, name_w);
                    p.galley(pos2(rect.left() + 26.0, rect.center().y - 5.0 - g.size().y / 2.0), g, Color32::WHITE);
                    // Size bar relative to the largest sibling.
                    let bar = Rect::from_min_size(pos2(rect.left() + 26.0, rect.center().y + 9.0), vec2((size_x - rect.left() - 26.0).max(10.0), 4.0));
                    p.rect_filled(bar, 2.0, TRACK);
                    let mut fill = bar;
                    fill.set_width((bar.width() * pct(c.size, top)).max(2.0));
                    p.rect_filled(fill, 2.0, color);

                    if c.real() {
                        let brect = Rect::from_min_max(pos2(rect.right() - buttons, rect.top()), rect.right_bottom());
                        let mut bui = ui.new_child(UiBuilder::new().max_rect(brect).layout(Layout::right_to_left(Align::Center)));
                        let path = c.path.clone().unwrap_or_default();
                        if !protected(&path) && icon_button(&mut bui, ic::TRASH, DANGER, "Move to Trash").clicked() {
                            *act = Some(Act::Trash(path.clone(), c.name.clone(), c.size));
                        }
                        if icon_button(&mut bui, ic::FOLDER_OPEN, DIM, "Show in Finder").clicked() {
                            *act = Some(Act::Reveal(path));
                        }
                    }
                    let resp = if drill { resp.on_hover_cursor(egui::CursorIcon::PointingHand) } else { resp };
                    let resp = match &c.path {
                        Some(path) => resp.on_hover_text(short(path)),
                        None => resp,
                    };
                    if resp.clicked() && drill {
                        *act = Some(Act::Focus(st.focus.iter().copied().chain([i]).collect()));
                    }
                }
            },
        );
        if focus.children.is_empty() {
            ui.label(RichText::new("Nothing inside.").color(DIM));
        }
        st.list_hover = list_hover;
    }

    fn changes_card(&self, ui: &mut Ui) {
        let Some(ch) = &self.diskmap.changes else { return };
        card(ui, |ui| {
            let now = unix_now();
            let Some(since) = ch.since else {
                ui.horizontal(|ui| {
                    badge(ui, ic::CLOCK_COUNTER_CLOCKWISE, ACCENT, 34.0);
                    ui.vertical(|ui| {
                        ui.label(RichText::new("What grew").strong().color(Color32::WHITE));
                        ui.label(RichText::new(format!("First snapshot saved {}. Scan again later to see which folders grew.", ago(now.saturating_sub(ch.latest)))).small().color(DIM));
                    });
                });
                return;
            };
            ui.horizontal(|ui| {
                badge(ui, ic::CLOCK_COUNTER_CLOCKWISE, ACCENT, 34.0);
                ui.vertical(|ui| {
                    ui.label(RichText::new(format!("Changes since {}", ago(now.saturating_sub(since)))).strong().color(Color32::WHITE));
                    ui.label(RichText::new("Folders in your home that grew or shrank between your last two scans.").small().color(DIM));
                });
            });
            ui.add_space(8.0);
            if ch.grew.is_empty() && ch.shrank.is_empty() {
                ui.label(RichText::new(format!("{}  Nothing changed by more than {}.", ic::CHECK_CIRCLE, human(MIN_CHANGE))).color(SUCCESS));
                return;
            }
            let max = ch.grew.first().map_or(1, |g| g.1).max(1) as f32;
            let row = |ui: &mut Ui, path: &str, text: String, color: Color32| {
                ui.horizontal(|ui| {
                    ui.add_sized(vec2(84.0, 18.0), Label::new(RichText::new(text).strong().color(color)));
                    ui.add(Label::new(RichText::new(format!("~/{path}")).color(Color32::from_rgb(210, 210, 216))).truncate());
                });
            };
            ui.columns(2, |cols| {
                cols[0].label(RichText::new(format!("{}  Grew", ic::TREND_UP)).color(DIM));
                for (p, d) in &ch.grew {
                    let t = (*d as f32 / max).sqrt();
                    let c = if t < 0.5 { SUCCESS.lerp_to_gamma(WARN, t * 2.0) } else { WARN.lerp_to_gamma(DANGER, t * 2.0 - 1.0) };
                    row(&mut cols[0], p, format!("+{}", human(*d as u64)), c);
                }
                if ch.grew.is_empty() {
                    cols[0].label(RichText::new("Nothing grew").small().color(DIM));
                }
                cols[1].label(RichText::new(format!("{}  Shrank", ic::TREND_DOWN)).color(DIM));
                for (p, d) in &ch.shrank {
                    row(&mut cols[1], p, format!("−{}", human(d.unsigned_abs())), ACCENT);
                }
                if ch.shrank.is_empty() {
                    cols[1].label(RichText::new("Nothing shrank").small().color(DIM));
                }
            });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(v: &[(&str, u64)]) -> HashMap<String, u64> {
        v.iter().map(|(k, s)| (k.to_string(), *s)).collect()
    }

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cleanyou-diskmap-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn tsv_roundtrip() {
        let m = map(&[("Library", 5_000_000_000), ("Library/Caches", 1_200_000), ("My Stuff/a b", 7)]);
        let s = to_tsv(&m);
        assert_eq!(parse_tsv(&s), m);
        assert_eq!(parse_tsv("garbage\n12\tok\nx\ty\n"), map(&[("ok", 12)]));
    }

    #[test]
    fn diff_points_at_specific_folder() {
        let old = map(&[("Library", 10_000 * MB), ("Library/Caches", 2_000 * MB), ("Library/Caches/X", 1_000 * MB), ("Movies", 500 * MB), ("Old", 300 * MB)]);
        let new = map(&[("Library", 13_000 * MB), ("Library/Caches", 4_900 * MB), ("Library/Caches/X", 3_900 * MB), ("Movies", 505 * MB), ("New", 50 * MB)]);
        let (grew, shrank) = diff(&old, &new, MIN_CHANGE);
        // Library and Library/Caches growth is explained by Library/Caches/X.
        assert_eq!(grew, vec![("Library/Caches/X".to_string(), 2_900 * MB as i64), ("New".to_string(), 50 * MB as i64)]);
        assert_eq!(shrank, vec![("Old".to_string(), -(300 * MB as i64))]);
    }

    #[test]
    fn diff_keeps_parent_when_spread_out() {
        let old = map(&[("A", 100 * MB), ("A/x", 50 * MB), ("A/y", 50 * MB)]);
        let new = map(&[("A", 200 * MB), ("A/x", 100 * MB), ("A/y", 100 * MB)]);
        let (grew, _) = diff(&old, &new, MIN_CHANGE);
        assert_eq!(grew[0], ("A".to_string(), 100 * MB as i64));
        assert_eq!(grew.len(), 3);
    }

    #[test]
    fn snapshots_keep_last_n() {
        let d = tmp("snap");
        for t in 1..=13u64 {
            save_snapshot(&d, t * 100, &map(&[("Dir", t * MB * 20)]), KEEP_SNAPSHOTS).unwrap();
        }
        let all = list_snapshots(&d);
        assert_eq!(all.len(), KEEP_SNAPSHOTS);
        assert_eq!(all[0].0, 400);
        assert_eq!(all.last().unwrap().0, 1300);
        let ch = load_changes(&d).unwrap();
        assert_eq!(ch.since, Some(1200));
        assert_eq!(ch.latest, 1300);
        assert_eq!(ch.grew, vec![("Dir".to_string(), 20 * MB as i64)]);
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn ago_text() {
        assert_eq!(ago(5), "just now");
        assert_eq!(ago(125), "2 min ago");
        assert_eq!(ago(3600), "1 hour ago");
        assert_eq!(ago(86_400 * 3), "3 days ago");
    }

    #[test]
    fn packages() {
        assert!(is_package("Xcode.app"));
        assert!(is_package("Photos Library.photoslibrary"));
        assert!(!is_package(".app"));
        assert!(!is_package("Documents"));
    }

    #[test]
    fn finish_merges_small_children() {
        let kids = vec![
            Node::leaf("a", None, 50, Kind::File),
            Node::leaf("b", None, 5, Kind::File),
            Node::leaf("c", None, 200, Kind::File),
            Node::leaf("d", None, 3, Kind::File),
        ];
        let n = finish("x".into(), None, 258, kids, 10);
        let names: Vec<_> = n.children.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["c", "a", "2 smaller items"]);
        assert_eq!(n.children[2].size, 8);
        assert!(n.has_smaller());
        // A single small child is kept as is.
        let n = finish("y".into(), None, 60, vec![Node::leaf("a", None, 50, Kind::File), Node::leaf("b", None, 5, Kind::File)], 10);
        assert!(!n.has_smaller());
    }

    #[test]
    fn builds_tree_from_disk() {
        let d = tmp("tree");
        fs::create_dir_all(d.join("big/inner")).unwrap();
        fs::write(d.join("big/inner/blob"), vec![1u8; 3_000_000]).unwrap();
        for i in 0..5 {
            fs::write(d.join(format!("big/tiny{i}")), b"hello").unwrap();
        }
        fs::create_dir_all(d.join("Thing.app/Contents")).unwrap();
        fs::write(d.join("Thing.app/Contents/bin"), vec![2u8; 1_500_000]).unwrap();
        std::os::unix::fs::symlink(d.join("big"), d.join("link")).unwrap();

        let prog = Progress::default();
        let (mut groups, snap) = scan_groups(vec![ls(&d)], 1_000_000, &prog, &d, true);
        let root = dir_node("root", d.clone(), groups.pop().unwrap(), 1_000_000);
        let by = |n: &Node, name: &str| n.children.iter().position(|c| c.name == name);

        let big = &root.children[by(&root, "big").unwrap()];
        assert_eq!(big.kind, Kind::Dir);
        assert!(big.size >= 3_000_000);
        assert_eq!(big.children[0].name, "inner");
        assert!(big.has_smaller(), "tiny files should be merged: {:?}", big.children);
        let app = &root.children[by(&root, "Thing.app").unwrap()];
        assert_eq!(app.kind, Kind::Package);
        assert!(app.children.is_empty() && app.size >= 1_500_000);
        // The symlink is not followed: a tiny file leaf.
        let link = &root.children[by(&root, "link").unwrap()];
        assert!(link.kind == Kind::File && link.size < 100_000);
        assert!(root.size >= big.size + app.size);
        assert!(snap.iter().any(|(k, s)| k == "big/inner" && *s >= 3_000_000));
        assert!(prog.files.load(Ordering::Relaxed) >= 9);

        // Layout covers the full circle at depth 1 and nests deeper rings inside parents.
        let segs = layout(&root);
        let first: f32 = segs.iter().filter(|s| s.depth == 1).map(|s| s.a1 - s.a0).sum();
        assert!(first <= TAU + 1e-3 && first > TAU * 0.9);
        for s in segs.iter().filter(|s| s.depth > 1) {
            let parent = segs.iter().find(|p| p.rel == s.rel[..s.rel.len() - 1]).unwrap();
            assert!(s.a0 >= parent.a0 - 1e-4 && s.a1 <= parent.a1 + 1e-4);
        }

        // Deleting a folder drops it and shrinks its ancestors.
        let mut st = State { root: Some(root), focus: vec![], ..Default::default() };
        let big_path = d.join("big/inner");
        let before = st.root.as_ref().unwrap().size;
        st.apply_removed(&HashSet::from([big_path]));
        let after = st.root.as_ref().unwrap().size;
        assert!(before - after >= 3_000_000);
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn hit_testing() {
        let root = finish("r".into(), None, 100, vec![Node::leaf("a", None, 75, Kind::File), Node::leaf("b", None, 25, Kind::File)], 0);
        let segs = layout(&root);
        let g = Geo::new(Rect::from_min_size(pos2(0.0, 0.0), vec2(208.0, 208.0)));
        assert!(matches!(hit(&segs, g, g.c), Some(Hit::Center)));
        let (ri, ro) = g.radii(1);
        let mid = (ri + ro) / 2.0;
        // Right of center (3 o'clock) is in "a" (first 75% clockwise from 12).
        assert!(matches!(hit(&segs, g, g.c + vec2(mid, 0.0)), Some(Hit::Seg(0))));
        // Left of center (9 o'clock) is the start of "b".
        assert!(matches!(hit(&segs, g, g.c + vec2(-mid, -1.0)), Some(Hit::Seg(1))));
        assert!(hit(&segs, g, g.c + vec2(0.0, g.r0 + g.w * 1.5)).is_none());
    }
}
