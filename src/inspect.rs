//! Inspector: everything about one file or folder, measured in the background.

use super::*;
use std::fs;
use std::os::macos::fs::MetadataExt as _;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// `st_flags` bit macOS sets on files stored with transparent (APFS/HFS) compression.
const UF_COMPRESSED: u32 = 0x20;
const LARGEST: usize = 8;

#[derive(Default, Clone)]
pub struct Info {
    /// Bytes the item actually takes on disk.
    pub on_disk: u64,
    /// Bytes its contents add up to (what Finder calls "size").
    pub logical: u64,
    /// Bytes saved by transparent compression.
    pub compressed_saved: u64,
    pub files: u64,
    pub folders: u64,
    /// Already formatted for display.
    pub created: Option<String>,
    pub modified: Option<String>,
    /// Largest items directly inside, biggest first: (path, bytes on disk).
    pub largest: Vec<(PathBuf, u64)>,
    pub done: bool,
}

#[derive(Default)]
struct Totals {
    on_disk: u64,
    logical: u64,
    saved: u64,
    files: u64,
    folders: u64,
}

impl Totals {
    fn add(&mut self, m: &fs::Metadata) {
        let disk = m.blocks() * 512;
        self.on_disk += disk;
        if m.is_dir() {
            self.folders += 1;
        } else {
            self.files += 1;
            self.logical += m.len();
            if m.st_flags() & UF_COMPRESSED != 0 {
                self.saved += m.len().saturating_sub(disk);
            }
        }
    }
}

fn date(t: std::io::Result<SystemTime>) -> Option<String> {
    let secs = t.ok()?.duration_since(UNIX_EPOCH).ok()?.as_secs();
    Some(disp(&format_date(secs))).filter(|s| !s.is_empty())
}

/// Walks `path` once, filling `out` as it goes so the panel fills in live.
fn measure(path: &Path, out: &Mutex<Info>, stop: &AtomicBool, ctx: &egui::Context) {
    let Ok(m) = fs::symlink_metadata(path) else {
        out.lock().unwrap().done = true;
        return;
    };
    {
        let mut o = out.lock().unwrap();
        o.created = date(m.created());
        o.modified = date(m.modified());
    }
    let mut total = Totals::default();
    total.add(&m);
    if !m.is_dir() {
        let mut o = out.lock().unwrap();
        (o.on_disk, o.logical, o.compressed_saved, o.files, o.done) = (total.on_disk, total.logical, total.saved, 1, true);
        return;
    }
    total.folders -= 1; // the folder itself isn't "inside"
    let kids: Vec<PathBuf> = fs::read_dir(path).map(|rd| rd.flatten().map(|e| e.path()).collect()).unwrap_or_default();
    let mut largest: Vec<(PathBuf, u64)> = Vec::new();
    let mut last = Instant::now();
    for kid in kids {
        let mut sub = Totals::default();
        for e in walkdir::WalkDir::new(&kid).follow_links(false).into_iter().flatten() {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            if let Ok(m) = e.metadata() {
                sub.add(&m);
            }
        }
        let kid_size = sub.on_disk;
        total.on_disk += sub.on_disk;
        total.logical += sub.logical;
        total.saved += sub.saved;
        total.files += sub.files;
        total.folders += sub.folders;
        largest.push((kid, kid_size));
        if last.elapsed() > Duration::from_millis(200) {
            last = Instant::now();
            largest.sort_by(|a, b| b.1.cmp(&a.1));
            largest.truncate(LARGEST);
            let mut o = out.lock().unwrap();
            (o.on_disk, o.logical, o.compressed_saved, o.files, o.folders) = (total.on_disk, total.logical, total.saved, total.files, total.folders);
            o.largest = largest.clone();
            ctx.request_repaint();
        }
    }
    largest.sort_by(|a, b| b.1.cmp(&a.1));
    largest.truncate(LARGEST);
    let mut o = out.lock().unwrap();
    (o.on_disk, o.logical, o.compressed_saved, o.files, o.folders) = (total.on_disk, total.logical, total.saved, total.files, total.folders);
    o.largest = largest;
    o.done = true;
    ctx.request_repaint();
}

#[derive(Default)]
pub struct State {
    pub target: Option<PathBuf>,
    /// The parent's (name, size) when the caller knows it, for "share of parent".
    parent: Option<(String, u64)>,
    info: Arc<Mutex<Info>>,
    stop: Arc<AtomicBool>,
}

pub enum Act {
    Inspect(PathBuf),
    Stage(PathBuf, String, u64),
    Reveal(PathBuf),
    Close,
}

impl State {
    /// Starts measuring `path` (stopping any earlier measurement).
    pub fn open(&mut self, path: PathBuf, parent: Option<(String, u64)>, ctx: &egui::Context) {
        if self.target.as_ref() == Some(&path) {
            return;
        }
        self.stop.store(true, Ordering::Relaxed);
        let (info, stop) = (Arc::new(Mutex::new(Info::default())), Arc::new(AtomicBool::new(false)));
        (self.info, self.stop, self.target, self.parent) = (info.clone(), stop.clone(), Some(path.clone()), parent);
        let ctx = ctx.clone();
        std::thread::spawn(move || measure(&path, &info, &stop, &ctx));
    }

    pub fn close(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.target = None;
    }

    /// The Inspector card. Returns what the user clicked.
    pub fn show(&self, ui: &mut Ui) -> Option<Act> {
        let path = self.target.as_ref()?;
        let info = self.info.lock().unwrap().clone();
        let mut act = None;
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string());
        card(ui, |ui| {
            ui.horizontal(|ui| {
                badge(ui, ic::INFO, ACCENT, 34.0);
                ui.vertical(|ui| {
                    ui.add(Label::new(RichText::new(disp(&name)).strong().color(Color32::WHITE)).truncate());
                    ui.add(Label::new(RichText::new(short(path)).small().color(DIM)).truncate());
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if icon_btn(ui, ic::X, "Close", false).clicked() {
                        act = Some(Act::Close);
                    }
                    if icon_btn(ui, ic::FOLDER_OPEN, "Show in Finder", false).clicked() {
                        act = Some(Act::Reveal(path.clone()));
                    }
                    if !scan::protected(path) && icon_btn(ui, ic::LIST_PLUS, "Add to Cleanup list", false).clicked() {
                        act = Some(Act::Stage(path.clone(), name.clone(), info.on_disk));
                    }
                    if !info.done {
                        ui.add(egui::Spinner::new().size(14.0).color(DIM));
                    }
                });
            });
            ui.add_space(10.0);
            let fact = |ui: &mut Ui, k: &str, v: String, tip: &str| {
                ui.horizontal(|ui| {
                    ui.add_sized(vec2(130.0, 18.0), Label::new(RichText::new(k).color(DIM)));
                    ui.label(RichText::new(v).color(Color32::WHITE)).on_hover_text(tip);
                });
            };
            fact(ui, "Size on disk", human(info.on_disk), "Space this really takes on your drive");
            fact(ui, "Size of contents", human(info.logical), "What the files add up to. Can be bigger than the size on disk when files are compressed or sparse");
            if info.compressed_saved > 0 {
                fact(ui, "Compression saved", human(info.compressed_saved), "macOS stores these files compressed, saving this much");
            }
            if path.is_dir() {
                fact(ui, "Inside", format!("{} files · {} folders", info.files, info.folders), "");
            }
            if let Some((pname, psize)) = &self.parent {
                let share = if *psize > 0 { info.on_disk as f64 / *psize as f64 * 100.0 } else { 0.0 };
                fact(ui, "Share of parent", format!("{share:.1}% of {pname}"), "");
            }
            let dash = |t: Option<String>| t.unwrap_or_else(|| "—".into());
            fact(ui, "Created", dash(info.created.clone()), "");
            fact(ui, "Modified", dash(info.modified.clone()), "");
            if !info.largest.is_empty() {
                ui.add_space(8.0);
                ui.label(RichText::new("Largest inside").small().strong().color(DIM));
                let top = info.largest[0].1.max(1);
                for (p, s) in &info.largest {
                    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 30.0), Sense::click());
                    if resp.hovered() {
                        ui.painter().rect_filled(rect, 6.0, Color32::from_white_alpha(8));
                    }
                    let n = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    let size_g = ui.painter().layout_no_wrap(human(*s), FontId::proportional(12.5), Color32::WHITE);
                    let sx = rect.right() - 6.0 - size_g.size().x;
                    ui.painter().galley(pos2(sx, rect.center().y - size_g.size().y / 2.0 - 3.0), size_g, Color32::WHITE);
                    let g = truncated(ui, &disp(&n), 12.5, Color32::WHITE, sx - rect.left() - 16.0);
                    ui.painter().galley(pos2(rect.left() + 6.0, rect.center().y - g.size().y / 2.0 - 3.0), g, Color32::WHITE);
                    let bar = Rect::from_min_size(pos2(rect.left() + 6.0, rect.bottom() - 6.0), vec2(rect.width() - 12.0, 3.0));
                    ui.painter().rect_filled(bar, 1.5, TRACK);
                    let mut fill = bar;
                    fill.set_width((bar.width() * (*s as f32 / top as f32)).max(2.0));
                    ui.painter().rect_filled(fill, 1.5, ACCENT);
                    if resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text("Inspect this").clicked() {
                        act = Some(Act::Inspect(p.clone()));
                    }
                }
            }
        });
        act
    }
}

/// "6 Oct 2026, 14:05" in local time (via the system's own formatter).
pub fn format_date(t: u64) -> String {
    std::process::Command::new("/bin/date")
        .args(["-r", &t.to_string(), "+%-d %b %Y, %H:%M"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measures_folder() {
        let d = std::env::temp_dir().join(format!("cleanyou-inspect-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(d.join("sub")).unwrap();
        fs::write(d.join("sub/big"), vec![7u8; 2_000_000]).unwrap();
        fs::write(d.join("small"), b"hi").unwrap();
        let out = Mutex::new(Info::default());
        measure(&d, &out, &AtomicBool::new(false), &egui::Context::default());
        let i = out.into_inner().unwrap();
        assert!(i.done);
        assert_eq!((i.files, i.folders), (2, 1));
        assert_eq!(i.logical, 2_000_002);
        assert!(i.on_disk >= 2_000_000);
        assert_eq!(i.largest[0].0, d.join("sub"));
        assert!(i.modified.is_some());
        fs::remove_dir_all(&d).unwrap();
    }
}
