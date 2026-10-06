use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use walkdir::WalkDir;

const MB: u64 = 1_000_000;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Cat {
    Junk,
    Dev,
    Ai,
    Large,
    Duplicates,
    Installers,
    Games,
    Recordings,
}

impl Cat {
    pub const ALL: [Cat; 8] =
        [Cat::Junk, Cat::Dev, Cat::Ai, Cat::Large, Cat::Duplicates, Cat::Installers, Cat::Games, Cat::Recordings];

    pub fn title(self) -> &'static str {
        match self {
            Cat::Junk => "System Junk",
            Cat::Dev => "Developer Junk",
            Cat::Ai => "Local AI Models",
            Cat::Large => "Large Files",
            Cat::Duplicates => "Duplicates",
            Cat::Installers => "Installers & Downloads",
            Cat::Games => "Game Library",
            Cat::Recordings => "Recordings Hub",
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            Cat::Junk => "App caches, logs and package-manager caches. Apps rebuild these automatically.",
            Cat::Dev => "node_modules, build folders (.next, Rust target), Python venvs and Pods. Reinstall any time.",
            Cat::Ai => "Local models from Ollama, LM Studio, Hugging Face, GPT4All and loose model files.",
            Cat::Large => "Single files and disk images above the size you choose.",
            Cat::Duplicates => "Identical copies of the same file (byte-for-byte). One copy of each is always kept.",
            Cat::Installers => "Leftover .dmg / .pkg installers and Downloads you haven't opened in 90+ days.",
            Cat::Games => "Game apps plus Steam, Epic, CrossOver and Minecraft libraries.",
            Cat::Recordings => "Screen recordings scattered around your Mac. Gather them in one folder or shrink them.",
        }
    }

    /// Regenerable data can be deleted for good; everything else goes to the Trash by default.
    pub fn regenerable(self) -> bool {
        matches!(self, Cat::Junk | Cat::Dev)
    }
}

#[derive(Clone)]
pub struct Item {
    pub cat: Cat,
    pub path: PathBuf,
    pub label: String,
    pub size: u64,
    pub selected: bool,
    /// Included in one-click Smart Clean.
    pub safe: bool,
    /// Unix seconds of the last time this (or, for dev folders, its project) was touched. 0 = unknown.
    pub last_used: u64,
    /// For duplicates: the identical copy that is kept.
    pub dup_of: Option<PathBuf>,
}

impl Item {
    /// Whole days since `last_used`, if known.
    pub fn idle_days(&self) -> Option<u64> {
        (self.last_used > 0).then(|| now_secs().saturating_sub(self.last_used) / 86_400)
    }
}

pub fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Latest of modified/accessed time for one path.
fn touched(p: &Path) -> u64 {
    fs::symlink_metadata(p).map(|m| (m.mtime().max(m.atime())).max(0) as u64).unwrap_or(0)
}

/// When a project was last worked on: newest modification among its own files
/// (a few levels deep), ignoring rebuildable folders like node_modules or .next.
fn project_activity(project: &Path) -> u64 {
    let mut newest = 0;
    let mut it = WalkDir::new(project).max_depth(4).follow_links(false).into_iter();
    let mut seen = 0;
    while let Some(Ok(e)) = it.next() {
        seen += 1;
        if seen > 5_000 {
            break;
        }
        let name = e.file_name().to_string_lossy();
        if e.depth() > 0 && e.file_type().is_dir() && dev_kind(&name, e.path()).is_some() {
            it.skip_current_dir();
            continue;
        }
        if let Ok(m) = e.metadata() {
            newest = newest.max(m.mtime().max(0) as u64);
        }
        if name == ".git" {
            it.skip_current_dir(); // its own mtime moves on every commit; the contents are noise
        }
    }
    newest
}

pub enum Msg {
    Status(String),
    Progress(String, u64),
    Items(Vec<Item>),
    PhaseDone(&'static str),
    ScanDone,
    Removed { path: PathBuf, err: Option<String>, before: u64, remaining: u64 },
    DeleteDone,
}

pub const PHASES: [&str; 5] = ["System junk", "AI models", "Games", "Your files", "Duplicates"];

pub fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/".into()))
}

pub fn human(b: u64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut f = b as f64;
    let mut i = 0;
    while f >= 1000.0 && i < units.len() - 1 {
        f /= 1000.0;
        i += 1;
    }
    if i == 0 { format!("{b} B") } else { format!("{f:.1} {}", units[i]) }
}

/// Actual bytes on disk (handles sparse files and iCloud placeholders).
fn on_disk(m: &fs::Metadata) -> u64 {
    m.blocks() * 512
}

pub fn dir_size(p: &Path) -> u64 {
    WalkDir::new(p)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter_map(|e| e.metadata().ok())
        .map(|m| on_disk(&m))
        .sum()
}

fn item(cat: Cat, path: PathBuf, label: impl Into<String>, size: u64) -> Item {
    let last_used = touched(&path);
    Item { cat, path, label: label.into(), size, selected: false, safe: false, last_used, dup_of: None }
}

fn name_of(p: &Path) -> String {
    p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
}

fn children(dir: &Path) -> Vec<PathBuf> {
    fs::read_dir(dir)
        .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.path()).collect())
        .unwrap_or_default()
}

pub fn is_recording(name: &str) -> bool {
    let lower = name.to_lowercase();
    let video = [".mov", ".mp4", ".m4v"].iter().any(|e| lower.ends_with(e));
    video
        && ["screen recording", "screen capture", "cleanshot", "screenrecording"]
            .iter()
            .any(|p| lower.starts_with(p))
}

pub fn recordings_dir() -> PathBuf {
    home().join("Movies/Screen Recordings")
}

fn ai_dirs(h: &Path) -> Vec<(PathBuf, &'static str)> {
    vec![
        (h.join(".ollama/models"), "Ollama models"),
        (h.join(".cache/huggingface"), "Hugging Face cache"),
        (h.join(".cache/lm-studio/models"), "LM Studio"),
        (h.join(".lmstudio/models"), "LM Studio"),
        (h.join("Library/Application Support/nomic.ai/GPT4All"), "GPT4All models"),
        (h.join("Library/Application Support/Jan/data/models"), "Jan models"),
        (h.join("jan/models"), "Jan models"),
        (h.join(".cache/torch"), "PyTorch hub cache"),
        (h.join(".cache/whisper"), "Whisper models"),
        (h.join("Library/Caches/llama.cpp"), "llama.cpp cache"),
        (h.join(".diffusionbee"), "DiffusionBee models"),
        (h.join("Library/Containers/com.liuliu.draw-things/Data/Documents/Models"), "Draw Things models"),
    ]
}

pub fn scan_all(tx: Sender<Msg>) {
    let jobs: [fn(Sender<Msg>); 3] = [scan_junk, scan_ai_and_games, scan_files];
    let handles: Vec<_> = jobs
        .into_iter()
        .map(|f| {
            let tx = tx.clone();
            std::thread::spawn(move || f(tx))
        })
        .collect();
    for h in handles {
        let _ = h.join();
    }
    let _ = tx.send(Msg::ScanDone);
}

/// Caches, logs and other junk with their sizes. `on_item` is called as each one is measured.
pub fn junk_items(mut on_item: impl FnMut(Item)) {
    let h = home();
    for p in children(&h.join("Library/Caches")) {
        let size = dir_size(&p);
        if size >= MB {
            let mut it = item(Cat::Junk, p.clone(), format!("{} cache", name_of(&p)), size);
            it.safe = true;
            on_item(it);
        }
    }
    let fixed = [
        ("Library/Logs", "User logs", true),
        ("Library/Developer/Xcode/DerivedData", "Xcode build data", true),
        ("Library/Developer/Xcode/iOS DeviceSupport", "Xcode device support files", true),
        ("Library/Developer/CoreSimulator/Caches", "iOS Simulator caches", true),
        ("Library/Application Support/Code/Cache", "VS Code cache", true),
        ("Library/Application Support/Code/CachedData", "VS Code cached data", true),
        ("Library/Application Support/Slack/Cache", "Slack cache", true),
        ("Library/Application Support/discord/Cache", "Discord cache", true),
        (".npm/_cacache", "npm cache", true),
        (".yarn/berry/cache", "Yarn cache", true),
        ("Library/pnpm/store", "pnpm store", true),
        (".cache/pip", "pip cache", true),
        (".gradle/caches", "Gradle cache", true),
        (".cargo/registry/cache", "Cargo download cache", true),
        (".Trash", "Trash", true),
    ];
    for (rel, label, safe) in fixed {
        let p = h.join(rel);
        if p.exists() {
            let size = dir_size(&p);
            if size >= MB {
                let mut it = item(Cat::Junk, p, label, size);
                it.safe = safe;
                on_item(it);
            }
        }
    }
}

fn scan_junk(tx: Sender<Msg>) {
    junk_items(|it| {
        let _ = tx.send(Msg::Items(vec![it]));
    });
    let _ = tx.send(Msg::PhaseDone(PHASES[0]));
}

/// One-click clean used by the menu bar and auto-clean: removes safe junk permanently
/// (never the Trash). Returns bytes freed.
pub fn quick_clean() -> u64 {
    let trash = home().join(".Trash");
    let mut freed = 0;
    junk_items(|it| {
        if it.safe && it.path != trash {
            let _ = remove(&it.path, true); // partly-locked caches still free what they can
            let remaining = if it.path.exists() { dir_size(&it.path) } else { 0 };
            freed += it.size.saturating_sub(remaining);
        }
    });
    log_freed(freed);
    freed
}

/// The size of everything Smart Clean would remove (no deleting).
pub fn quick_clean_size() -> u64 {
    let trash = home().join(".Trash");
    let mut total = 0;
    junk_items(|it| {
        if it.safe && it.path != trash {
            total += it.size;
        }
    });
    total
}

fn scan_ai_and_games(tx: Sender<Msg>) {
    let h = home();
    let send = |v: Vec<Item>| {
        let v: Vec<Item> = v.into_iter().filter(|i| i.size >= MB).collect();
        if !v.is_empty() {
            let _ = tx.send(Msg::Items(v));
        }
    };

    for (dir, label) in ai_dirs(&h) {
        if !dir.exists() {
            continue;
        }
        let hub = dir.join("hub");
        if label.starts_with("Hugging Face") && hub.exists() {
            for p in children(&hub) {
                let n = name_of(&p);
                if let Some(model) = n.strip_prefix("models--").or(n.strip_prefix("datasets--")) {
                    let size = dir_size(&p);
                    send(vec![item(Cat::Ai, p.clone(), format!("{} (Hugging Face)", model.replace("--", "/")), size)]);
                }
            }
        } else if label.starts_with("LM Studio") {
            for publisher in children(&dir) {
                for model in children(&publisher) {
                    let size = dir_size(&model);
                    let label = format!("{}/{} (LM Studio)", name_of(&publisher), name_of(&model));
                    send(vec![item(Cat::Ai, model, label, size)]);
                }
            }
        } else {
            let size = dir_size(&dir);
            send(vec![item(Cat::Ai, dir.clone(), label, size)]);
        }
    }
    let _ = tx.send(Msg::PhaseDone(PHASES[1]));

    for apps in [PathBuf::from("/Applications"), h.join("Applications")] {
        for app in children(&apps) {
            if app.extension().map_or(false, |e| e == "app") && is_game(&app) {
                let label = name_of(&app).trim_end_matches(".app").to_string();
                send(vec![item(Cat::Games, app.clone(), label, dir_size(&app))]);
            }
        }
    }
    let libs: [(PathBuf, &str); 3] = [
        (h.join("Library/Application Support/Steam/steamapps/common"), "Steam"),
        (PathBuf::from("/Users/Shared/Epic Games"), "Epic Games"),
        (h.join("Library/Application Support/CrossOver/Bottles"), "CrossOver"),
    ];
    for (dir, store) in libs {
        for g in children(&dir) {
            if g.is_dir() {
                send(vec![item(Cat::Games, g.clone(), format!("{} ({store})", name_of(&g)), dir_size(&g))]);
            }
        }
    }
    let mc = h.join("Library/Application Support/minecraft");
    if mc.exists() {
        send(vec![item(Cat::Games, mc.clone(), "Minecraft worlds & data", dir_size(&mc))]);
    }
    let _ = tx.send(Msg::PhaseDone(PHASES[2]));
}

/// Recognises rebuildable developer folders. Returns a short kind label.
fn dev_kind(name: &str, path: &Path) -> Option<&'static str> {
    let parent = path.parent()?;
    let has = |f: &str| parent.join(f).exists();
    match name {
        "node_modules" => Some("node_modules"),
        ".next" | ".nuxt" | ".turbo" | ".svelte-kit" | ".parcel-cache" if has("package.json") => Some("build cache"),
        "target" if has("Cargo.toml") => Some("Rust build"),
        ".venv" | "venv" | "env" if path.join("pyvenv.cfg").exists() => Some("Python venv"),
        "Pods" if has("Podfile") => Some("CocoaPods"),
        _ => None,
    }
}

/// Walks the home folder with several threads (one per top-level folder).
fn scan_files(tx: Sender<Msg>) {
    let h = home();
    let skip: HashSet<PathBuf> = ai_dirs(&h).into_iter().map(|(p, _)| p).collect();
    let files = AtomicU64::new(0);
    let dup_candidates: Mutex<Vec<(u64, PathBuf)>> = Mutex::new(Vec::new());

    let docker = h.join("Library/Containers/com.docker.docker/Data/vms/0/data/Docker.raw");
    if let Ok(m) = fs::metadata(&docker) {
        let _ = tx.send(Msg::Items(vec![item(Cat::Large, docker, "Docker disk image", on_disk(&m))]));
    }

    let mut roots = Vec::new();
    for p in children(&h) {
        let n = name_of(&p);
        if n == "Library" || n == ".Trash" || skip.contains(&p) {
            continue;
        }
        roots.push(p);
    }
    let queue = Arc::new(Mutex::new(roots));

    std::thread::scope(|s| {
        for _ in 0..6 {
            let (queue, tx, h, skip, files, dups) = (queue.clone(), tx.clone(), &h, &skip, &files, &dup_candidates);
            s.spawn(move || loop {
                let Some(root) = queue.lock().unwrap().pop() else { break };
                walk(&root, h, skip, files, dups, &tx);
            });
        }
    });
    let _ = tx.send(Msg::PhaseDone(PHASES[3]));

    let groups = find_duplicates(dup_candidates.into_inner().unwrap_or_default());
    let mut items = Vec::new();
    for group in groups {
        // Keep the first (original-looking, oldest) copy, offer the rest.
        let original = &group[0];
        for copy in &group[1..] {
            let size = fs::metadata(copy).map(|m| on_disk(&m)).unwrap_or(0);
            let mut it = item(Cat::Duplicates, copy.clone(), name_of(copy), size);
            it.dup_of = Some(original.clone());
            items.push(it);
        }
    }
    if !items.is_empty() {
        let _ = tx.send(Msg::Items(items));
    }
    let _ = tx.send(Msg::PhaseDone(PHASES[4]));
}

fn walk(root: &Path, h: &Path, skip: &HashSet<PathBuf>, files: &AtomicU64, dups: &Mutex<Vec<(u64, PathBuf)>>, tx: &Sender<Msg>) {
    const INSTALLER_EXT: [&str; 5] = ["dmg", "pkg", "mpkg", "xip", "iso"];
    let downloads = h.join("Downloads");
    let mut local_dups = Vec::new();
    const MODEL_EXT: [&str; 9] = ["gguf", "ggml", "safetensors", "ckpt", "onnx", "mlmodel", "pth", "pt", "llamafile"];
    const PKG_SKIP: [&str; 9] =
        ["photoslibrary", "musiclibrary", "tvlibrary", "imovielibrary", "fcpbundle", "app", "logicx", "band", "photolibrary"];
    const PKG_VM: [&str; 4] = ["utm", "vmwarevm", "pvm", "sparsebundle"];

    let mut batch: Vec<Item> = Vec::new();
    let mut last = Instant::now();
    let mut it = WalkDir::new(root).follow_links(false).into_iter();
    while let Some(entry) = it.next() {
        let Ok(e) = entry else { continue };
        let name = e.file_name().to_string_lossy();
        let path = e.path();

        if last.elapsed().as_millis() > 150 {
            last = Instant::now();
            let rel = path.strip_prefix(h).unwrap_or(path);
            let _ = tx.send(Msg::Progress(format!("~/{}", rel.display()), files.load(Ordering::Relaxed)));
            if !batch.is_empty() {
                let _ = tx.send(Msg::Items(std::mem::take(&mut batch)));
            }
        }

        if e.file_type().is_dir() {
            if name == ".git" || skip.contains(path) {
                it.skip_current_dir();
                continue;
            }
            if let Some(kind) = dev_kind(&name, path) {
                it.skip_current_dir();
                // Ignore tool-owned installs like ~/.nvm or ~/.vscode/extensions.
                let parent = path.parent().unwrap_or(path);
                let rel = parent.strip_prefix(h).unwrap_or(parent);
                let hidden = rel.components().any(|c| c.as_os_str().to_string_lossy().starts_with('.'));
                let size = dir_size(path);
                if !hidden && size >= MB {
                    let label = format!("{} · {kind}", name_of(parent));
                    let mut it = item(Cat::Dev, path.to_path_buf(), label, size);
                    it.last_used = project_activity(parent);
                    batch.push(it);
                }
                continue;
            }
            let ext = path.extension().map(|x| x.to_string_lossy().to_lowercase()).unwrap_or_default();
            if PKG_SKIP.contains(&ext.as_str()) {
                it.skip_current_dir();
            } else if PKG_VM.contains(&ext.as_str()) {
                it.skip_current_dir();
                batch.push(item(Cat::Large, path.to_path_buf(), name.to_string(), dir_size(path)));
            } else if ext == "mlpackage" {
                it.skip_current_dir();
                batch.push(item(Cat::Ai, path.to_path_buf(), name.to_string(), dir_size(path)));
            }
            continue;
        }
        if !e.file_type().is_file() {
            continue;
        }
        files.fetch_add(1, Ordering::Relaxed);
        let Ok(m) = e.metadata() else { continue };
        let size = on_disk(&m);
        let ext = path.extension().map(|x| x.to_string_lossy().to_lowercase()).unwrap_or_default();

        // Files under hidden folders (~/.bun, ~/.local/bin, caches) belong to tools; never offer them as duplicates.
        if m.len() >= MB && !path.strip_prefix(h).unwrap_or(path).components().any(|c| c.as_os_str().to_string_lossy().starts_with('.')) {
            local_dups.push((m.len(), path.to_path_buf()));
        }
        if is_recording(&name) {
            batch.push(item(Cat::Recordings, path.to_path_buf(), name.to_string(), size));
        } else if MODEL_EXT.contains(&ext.as_str()) && size >= 20 * MB {
            batch.push(item(Cat::Ai, path.to_path_buf(), name.to_string(), size));
        } else if INSTALLER_EXT.contains(&ext.as_str()) && size >= MB {
            batch.push(item(Cat::Installers, path.to_path_buf(), format!("Installer · {name}"), size));
        } else if let Some(days) = unused_days(&m).filter(|d| *d >= 90 && size >= MB && path.starts_with(&downloads)) {
            batch.push(item(Cat::Installers, path.to_path_buf(), format!("{name} · unused for {days} days"), size));
        } else if size >= 100 * MB {
            batch.push(item(Cat::Large, path.to_path_buf(), name.to_string(), size));
        }
    }
    if !batch.is_empty() {
        let _ = tx.send(Msg::Items(batch));
    }
    dups.lock().unwrap().extend(local_dups);
}

/// Days since the file was last opened or changed (whichever is more recent).
fn unused_days(m: &fs::Metadata) -> Option<u64> {
    let last = m.atime().max(m.mtime()).max(0) as u64;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
    Some(now.saturating_sub(last) / 86_400)
}

fn hash_file(path: &Path, limit: Option<u64>) -> Option<u64> {
    use std::hash::Hasher;
    use std::io::Read;
    let f = fs::File::open(path).ok()?;
    let mut r: Box<dyn Read> = match limit {
        Some(n) => Box::new(f.take(n)),
        None => Box::new(f),
    };
    let mut h = std::collections::hash_map::DefaultHasher::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = r.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        h.write(&buf[..n]);
    }
    Some(h.finish())
}

/// Groups byte-identical files: same size → same first 64 KB → same full hash.
/// Each group is sorted keep-first: names that don't look like copies, then oldest, then shortest path.
/// Hard links of one file are not duplicates.
pub fn find_duplicates(mut files: Vec<(u64, PathBuf)>) -> Vec<Vec<PathBuf>> {
    use std::collections::HashMap;
    files.sort();
    let mut by_size: HashMap<u64, Vec<PathBuf>> = HashMap::new();
    for (len, p) in files {
        by_size.entry(len).or_default().push(p);
    }
    let mut groups = Vec::new();
    for (_, paths) in by_size.into_iter().filter(|(_, v)| v.len() > 1) {
        // Drop hard links (same inode).
        let mut seen = HashSet::new();
        let paths: Vec<PathBuf> =
            paths.into_iter().filter(|p| fs::metadata(p).map_or(false, |m| seen.insert((m.dev(), m.ino())))).collect();
        let mut by_head: HashMap<u64, Vec<PathBuf>> = HashMap::new();
        for p in paths {
            if let Some(h) = hash_file(&p, Some(64 * 1024)) {
                by_head.entry(h).or_default().push(p);
            }
        }
        for (_, paths) in by_head.into_iter().filter(|(_, v)| v.len() > 1) {
            let mut by_full: HashMap<u64, Vec<PathBuf>> = HashMap::new();
            for p in paths {
                if let Some(h) = hash_file(&p, None) {
                    by_full.entry(h).or_default().push(p);
                }
            }
            for (_, mut g) in by_full.into_iter().filter(|(_, v)| v.len() > 1) {
                g.sort_by_key(|p| {
                    (looks_like_copy(p), fs::metadata(p).map(|m| m.mtime()).unwrap_or(i64::MAX), p.as_os_str().len())
                });
                groups.push(g);
            }
        }
    }
    groups
}

/// "x copy.pdf", "x copy 2.pdf", "x (1).zip", or anything inside a "… copy" folder.
fn looks_like_copy(p: &Path) -> bool {
    p.iter().any(|c| {
        let c = c.to_string_lossy().to_lowercase();
        let stem = c.rsplit_once('.').map_or(c.as_str(), |(s, _)| s).trim_end();
        stem.ends_with(" copy")
            || stem.contains(" copy ")
            || stem.strip_suffix(')').and_then(|s| s.rsplit_once(" (")).is_some_and(|(_, n)| n.parse::<u32>().is_ok())
    })
}

fn is_game(app: &Path) -> bool {
    plist::Value::from_file(app.join("Contents/Info.plist"))
        .ok()
        .and_then(|v| {
            v.as_dictionary()?
                .get("LSApplicationCategoryType")?
                .as_string()
                .map(|s| s.contains("games"))
        })
        .unwrap_or(false)
}

/// Deletes everything it can inside `path`, skipping files that are locked or protected,
/// instead of giving up at the first error like `remove_dir_all`.
fn force_remove(path: &Path) -> Result<(), String> {
    if !path.is_dir() || path.is_symlink() {
        return fs::remove_file(path).map_err(|e| e.to_string());
    }
    let mut failed = 0;
    for e in WalkDir::new(path).follow_links(false).contents_first(true).into_iter().filter_map(|e| e.ok()) {
        let r = if e.file_type().is_dir() { fs::remove_dir(e.path()) } else { fs::remove_file(e.path()) };
        if r.is_err() && e.file_type().is_file() {
            failed += 1;
        }
    }
    if failed == 0 || !path.exists() {
        Ok(())
    } else {
        Err(format!("{failed} files are in use or protected by macOS"))
    }
}

/// Move to Trash, or delete for good when `permanent` is set.
pub fn remove(path: &Path, permanent: bool) -> Result<(), String> {
    if permanent {
        return force_remove(path);
    }
    use trash::macos::{DeleteMethod, TrashContextExtMacos};
    let mut ctx = trash::TrashContext::default();
    ctx.set_delete_method(DeleteMethod::NsFileManager);
    ctx.delete(path).map_err(|e| e.to_string())
}

/// The Trash itself can only be emptied, so its contents are always removed permanently.
pub fn remove_item(path: &Path, permanent: bool) -> Result<(), String> {
    if path == home().join(".Trash") {
        let errs: Vec<String> = children(path).iter().filter_map(|c| force_remove(c).err()).collect();
        return if errs.is_empty() { Ok(()) } else { Err(errs[0].clone()) };
    }
    remove(path, permanent)
}

/// Appends one "unix-time<TAB>bytes" line to the cleanup history (for the weekly report).
pub fn log_freed(bytes: u64) {
    if bytes == 0 {
        return;
    }
    let dir = home().join("Library/Application Support/CleanYou");
    let _ = fs::create_dir_all(&dir);
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    use std::io::Write;
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(dir.join("history.log")) {
        let _ = writeln!(f, "{now}\t{bytes}");
    }
}

/// (unix-time, bytes) entries of everything cleaned so far.
pub fn history() -> Vec<(u64, u64)> {
    fs::read_to_string(home().join("Library/Application Support/CleanYou/history.log"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let (t, b) = l.split_once('\t')?;
            Some((t.parse().ok()?, b.parse().ok()?))
        })
        .collect()
}

/// Paths that are never offered for removal from the Disk Map, Find or the Cleanup list:
/// the system, top-level folders, volume roots, home folders and the core ~/Library folders.
pub fn protected(p: &Path) -> bool {
    let h = home();
    let keep = ["Library", "Library/Application Support", "Library/Containers", "Library/Group Containers", "Library/Preferences", "Library/Mobile Documents"];
    let system = ["/System", "/bin", "/sbin", "/usr", "/private", "/etc", "/var", "/tmp", "/dev", "/Library", "/cores"];
    let top = |x: &Path| [Path::new("/"), Path::new("/Volumes"), Path::new("/Users")].contains(&x);
    !p.is_absolute()
        || p == h
        || p.parent().map_or(true, top)
        || (system.iter().any(|s| p.starts_with(s)) && !p.starts_with("/usr/local"))
        || keep.iter().any(|k| p == h.join(k))
        || p.starts_with(h.join("Library/Application Support/CleanYou"))
}

pub fn unique(dir: &Path, stem: &str, ext: &str) -> PathBuf {
    let mut p = dir.join(format!("{stem}.{ext}"));
    let mut n = 2;
    while p.exists() {
        p = dir.join(format!("{stem} {n}.{ext}"));
        n += 1;
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deletes_permanently_and_to_trash() {
        let base = std::env::temp_dir().join("cleanyou-test");
        let _ = fs::remove_dir_all(&base);
        let a = base.join("proj/node_modules/pkg/lib");
        fs::create_dir_all(&a).unwrap();
        fs::write(a.join("x.js"), vec![0u8; 50_000]).unwrap();
        // A read-only folder: its file can't be removed, the rest should still go.
        let locked = base.join("cache/locked");
        fs::create_dir_all(&locked).unwrap();
        fs::write(locked.join("f"), b"x").unwrap();
        fs::write(base.join("cache/free.bin"), vec![0u8; 50_000]).unwrap();
        let mut p = fs::metadata(&locked).unwrap().permissions();
        use std::os::unix::fs::PermissionsExt;
        p.set_mode(0o555);
        fs::set_permissions(&locked, p.clone()).unwrap();

        assert!(remove_item(&base.join("proj/node_modules"), true).is_ok());
        assert!(!base.join("proj/node_modules").exists());

        assert!(remove_item(&base.join("cache"), true).is_err());
        assert!(!base.join("cache/free.bin").exists(), "unlocked files are still removed");

        p.set_mode(0o755);
        fs::set_permissions(&locked, p).unwrap();
        fs::create_dir_all(base.join("t")).unwrap();
        assert!(remove(&base.join("t"), false).is_ok(), "move to Trash");
        assert!(!base.join("t").exists());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn finds_duplicates() {
        let base = std::env::temp_dir().join("cleanyou-dups");
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        let data: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let mut other = data.clone();
        *other.last_mut().unwrap() ^= 1; // same size and head, different tail
        fs::write(base.join("a.bin"), &data).unwrap();
        fs::write(base.join("b.bin"), &data).unwrap();
        fs::write(base.join("c.bin"), &other).unwrap();
        fs::hard_link(base.join("a.bin"), base.join("a-link.bin")).unwrap();
        let files = ["a.bin", "b.bin", "c.bin", "a-link.bin"].iter().map(|n| (200_000, base.join(n))).collect();
        let groups = find_duplicates(files);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].len(), 2, "a + b; c differs, the hard link isn't a copy");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn spots_copies() {
        for p in ["a/x copy.pdf", "a/x copy 2.pdf", "a/x (1).zip", "Downloads/Project Assignment 2 copy/f.bin"] {
            assert!(looks_like_copy(Path::new(p)), "{p}");
        }
        for p in ["Downloads/Project Assignment 2/f.bin", "a/IELTS 19 Audio.zip", "a/notes (draft).txt"] {
            assert!(!looks_like_copy(Path::new(p)), "{p}");
        }
    }
}
